//! Per-trace calc (data conversion, binning, stats), cross-trace layout
//! (bar grouping/stacking, area stacking) and drawing.

use std::collections::HashMap;

use serde_json::Value;

use super::axis::{AxisType, ExtOpts, PadSpec};
use super::{Axis, Ctx, Kind, Trace, push_note};
use crate::color::{Colorscale, Rgba};
use crate::format::{self, ExpFormat};
use crate::svg::{Anchor, PathData, Style, Svg, TextStyle, fmt_num};
use crate::text;
use crate::ticks;
use crate::time;
use crate::util::{self, Arr, Datum};

/// Above this many markers a trace switches to compact path output.
const COMPACT_MARKERS: usize = 4000;
/// Hard cap on drawn markers per trace (after pixel de-duplication).
const MAX_MARKERS: usize = 150_000;

pub enum Calc {
    None,
    Pending(String, String),
    Scatter(Box<ScatterCalc>),
    Bars(Box<BarCalc>),
    Box(Box<BoxCalc>),
    Heatmap(Box<HeatCalc>),
    Image(Box<ImageCalc>),
    Pie(Box<super::pie::PieCalc>),
}

pub struct ImageCalc {
    pub href: String,
    /// Extents in axis (l) units: first / last pixel edges.
    pub x: (f64, f64),
    pub y: (f64, f64),
}

// ---------------------------------------------------------------- types

#[derive(Clone, Debug)]
pub enum Each<T: Clone + Default> {
    One(T),
    Many(Vec<T>),
}

impl<T: Clone + Default> Each<T> {
    /// Value for point `i` (short arrays repeat their first value; an
    /// empty array yields the type's default).
    pub fn at(&self, i: usize) -> T {
        match self {
            Each::One(v) => v.clone(),
            Each::Many(v) => v.get(i).or_else(|| v.first()).cloned().unwrap_or_default(),
        }
    }
    pub fn first(&self) -> T {
        self.at(0)
    }
    pub fn is_many(&self) -> bool {
        matches!(self, Each::Many(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    Circle,
    Square,
    Diamond,
    Cross,
    X,
    TriUp,
    TriDown,
    TriLeft,
    TriRight,
    Pentagon,
    Hexagon,
    Star,
    LineNs,
    LineEw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Symbol {
    pub shape: Shape,
    pub open: bool,
}

impl Default for Symbol {
    fn default() -> Self {
        Symbol::CIRCLE
    }
}

impl Symbol {
    pub const CIRCLE: Symbol = Symbol { shape: Shape::Circle, open: false };

    pub fn parse(v: &Value) -> Symbol {
        const BY_NUM: [Shape; 13] = [
            Shape::Circle,
            Shape::Square,
            Shape::Diamond,
            Shape::Cross,
            Shape::X,
            Shape::TriUp,
            Shape::TriDown,
            Shape::TriLeft,
            Shape::TriRight,
            Shape::TriUp,
            Shape::TriDown,
            Shape::TriUp,
            Shape::TriDown,
        ];
        match v {
            Value::Number(n) => {
                let k = n.as_f64().unwrap_or(0.).max(0.) as usize;
                let base = k % 100;
                Symbol {
                    shape: match base {
                        13 => Shape::Pentagon,
                        14 | 15 => Shape::Hexagon,
                        17 => Shape::Star,
                        b if b < BY_NUM.len() => BY_NUM[b],
                        _ => Shape::Circle,
                    },
                    open: matches!(k / 100, 1 | 3),
                }
            }
            Value::String(s) => {
                let open = s.contains("-open");
                let base = s.split("-open").next().unwrap_or("").trim_end_matches("-dot");
                let shape = match base {
                    "square" => Shape::Square,
                    "diamond" | "diamond-tall" | "diamond-wide" => Shape::Diamond,
                    "cross" | "cross-thin" => Shape::Cross,
                    "x" | "x-thin" => Shape::X,
                    "triangle-up" | "triangle-ne" | "triangle-nw" => Shape::TriUp,
                    "triangle-down" | "triangle-se" | "triangle-sw" => Shape::TriDown,
                    "triangle-left" => Shape::TriLeft,
                    "triangle-right" => Shape::TriRight,
                    "pentagon" => Shape::Pentagon,
                    "hexagon" | "hexagon2" | "octagon" => Shape::Hexagon,
                    "star" | "hexagram" | "star-triangle-up" | "star-square" | "star-diamond" => Shape::Star,
                    "line-ns" => Shape::LineNs,
                    "line-ew" => Shape::LineEw,
                    _ => {
                        if let Some(n) = util::parse_num(s) {
                            return Symbol::parse(&Value::from(n));
                        }
                        Shape::Circle
                    }
                };
                Symbol { shape, open }
            }
            _ => Symbol::CIRCLE,
        }
    }

    /// plotly's symbol path for radius `r` centered at (x, y).
    pub fn path(&self, d: &mut PathData, x: f64, y: f64, r: f64) {
        let p = |d: &mut PathData, pts: &[(f64, f64)]| {
            for (i, &(px, py)) in pts.iter().enumerate() {
                if i == 0 {
                    d.move_to(x + px, y + py);
                } else {
                    d.line_to(x + px, y + py);
                }
            }
            d.close();
        };
        match self.shape {
            Shape::Circle => {
                d.move_to(x + r, y);
                d.arc_to(r, true, true, x - r, y);
                d.arc_to(r, true, true, x + r, y);
                d.close();
            }
            Shape::Square => p(d, &[(r, r), (-r, r), (-r, -r), (r, -r)]),
            Shape::Diamond => {
                let rd = r * 1.3;
                p(d, &[(rd, 0.), (0., rd), (-rd, 0.), (0., -rd)])
            }
            Shape::Cross => {
                let (a, b) = (r * 0.4, r * 1.2);
                p(d, &[(b, a), (a, a), (a, b), (-a, b), (-a, a), (-b, a), (-b, -a), (-a, -a), (-a, -b), (a, -b), (a, -a), (b, -a)])
            }
            Shape::X => {
                let rx = r * 0.8 / std::f64::consts::SQRT_2;
                let (a, b) = (rx, 2. * rx);
                p(d, &[(0., a), (a, b), (b, a), (a, 0.), (b, -a), (a, -b), (0., -a), (-a, -b), (-b, -a), (-a, 0.), (-b, a), (-a, b)])
            }
            Shape::TriUp | Shape::TriDown | Shape::TriLeft | Shape::TriRight => {
                let rt = r * 2. / 3f64.sqrt();
                let r2 = r / 2.;
                match self.shape {
                    Shape::TriUp => p(d, &[(-rt, r2), (rt, r2), (0., -r)]),
                    Shape::TriDown => p(d, &[(-rt, -r2), (rt, -r2), (0., r)]),
                    Shape::TriLeft => p(d, &[(r2, -rt), (r2, rt), (-r, 0.)]),
                    _ => p(d, &[(-r2, -rt), (-r2, rt), (r, 0.)]),
                }
            }
            Shape::Pentagon | Shape::Hexagon => {
                let n = if self.shape == Shape::Pentagon { 5 } else { 6 };
                let rr = r * if n == 5 { 1.05 } else { 1. };
                let pts: Vec<(f64, f64)> = (0..n)
                    .map(|k| {
                        let a = -std::f64::consts::FRAC_PI_2 + k as f64 * 2. * std::f64::consts::PI / n as f64;
                        (rr * a.cos(), rr * a.sin())
                    })
                    .collect();
                p(d, &pts)
            }
            Shape::Star => {
                let pts: Vec<(f64, f64)> = (0..10)
                    .map(|k| {
                        let rr = if k % 2 == 0 { r * 1.4 } else { r * 0.56 };
                        let a = -std::f64::consts::FRAC_PI_2 + k as f64 * std::f64::consts::PI / 5.;
                        (rr * a.cos(), rr * a.sin())
                    })
                    .collect();
                p(d, &pts)
            }
            Shape::LineNs => {
                d.move_to(x, y + r);
                d.line_to(x, y - r);
            }
            Shape::LineEw => {
                d.move_to(x + r, y);
                d.line_to(x - r, y);
            }
        }
    }

    fn is_line(&self) -> bool {
        matches!(self.shape, Shape::LineNs | Shape::LineEw)
    }
}

#[derive(Clone, Debug)]
pub struct Marker {
    pub color: Each<Rgba>,
    /// Radius in px.
    pub radius: Each<f64>,
    pub ppad: PadSpec,
    pub symbol: Each<Symbol>,
    pub opacity: Each<f64>,
    pub line_color: Each<Rgba>,
    pub line_width: f64,
    pub array_size: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineShape {
    Linear,
    Spline,
    Hv,
    Vh,
    Hvh,
    Vhv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    None,
    ToZeroY,
    ToZeroX,
    ToNextY,
    ToNextX,
    ToSelf,
    ToNext,
}

#[derive(Clone, Debug)]
pub struct ErrBars {
    pub plus: Vec<f64>,
    pub minus: Vec<f64>,
    pub color: Rgba,
    pub thickness: f64,
    pub width: f64,
}

pub struct ScatterCalc {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    pub lines: bool,
    pub markers: bool,
    pub text: bool,
    pub line_color: Rgba,
    pub line_width: f64,
    pub dash: Option<String>,
    pub shape: LineShape,
    pub connectgaps: bool,
    pub marker: Marker,
    pub fill: Fill,
    pub fillcolor: Rgba,
    pub stackgroup: Option<String>,
    pub groupnorm: String,
    /// Lower boundary for stacked fills.
    pub base: Option<Vec<f64>>,
    /// Previous trace's line (for tonexty/tonextx fills).
    pub next_to: Option<(Vec<f64>, Vec<f64>)>,
    pub texts: Vec<String>,
    pub textpos: String,
    pub text_size: f64,
    pub text_color: Rgba,
    pub err_x: Option<ErrBars>,
    pub err_y: Option<ErrBars>,
}

pub struct BarCalc {
    pub pos: Vec<f64>,
    pub val: Vec<f64>,
    pub base: Vec<f64>,
    pub explicit_base: bool,
    pub width: Option<Each<f64>>,
    pub offset: Option<Each<f64>>,
    pub color: Each<Rgba>,
    pub line_color: Each<Rgba>,
    pub line_width: f64,
    pub opacity: f64,
    pub texts: Vec<String>,
    pub textposition: String,
    pub textfont_size: f64,
    pub textfont_color: Option<Rgba>,
    pub hist: Option<HistInput>,
    pub offsetgroup: String,
    /// Bar extents after positioning: position axis (p0, p1), size axis (s0, s1).
    pub p0: Vec<f64>,
    pub p1: Vec<f64>,
    pub s0: Vec<f64>,
    pub s1: Vec<f64>,
}

pub struct HistInput {
    pub data: Vec<f64>,
    pub weights: Option<Vec<f64>>,
    pub func: String,
    pub norm: String,
    pub nbins: Option<f64>,
    pub start: Option<Value>,
    pub end: Option<Value>,
    pub size: Option<Value>,
    pub cumulative: bool,
    pub bingroup: String,
    pub categorical: bool,
}

pub struct BoxStat {
    pub pos: f64,
    pub q1: f64,
    pub med: f64,
    pub q3: f64,
    pub lf: f64,
    pub uf: f64,
    pub mean: f64,
    pub sd: f64,
    pub points: Vec<f64>,
    pub outliers: Vec<f64>,
}

pub struct BoxCalc {
    pub boxes: Vec<BoxStat>,
    pub line_color: Rgba,
    pub line_width: f64,
    pub fillcolor: Rgba,
    pub marker_color: Rgba,
    pub marker_radius: f64,
    pub boxpoints: String,
    pub boxmean: String,
    pub whiskerwidth: f64,
    pub jitter: f64,
    pub pointpos: f64,
    pub offsetgroup: String,
    /// After positioning (position units).
    pub width: f64,
    pub offset: f64,
    /// Candlestick / OHLC: per-box (line, fill) colors, width as a fraction
    /// of the position spacing, and OHLC tick drawing.
    pub colors: Option<Vec<(Rgba, Rgba)>>,
    pub width_frac: Option<f64>,
    pub ohlc: bool,
}

pub struct HeatCalc {
    pub xe: Vec<f64>,
    pub ye: Vec<f64>,
    pub z: Vec<Vec<Option<f64>>>,
    pub zmin: f64,
    pub zmax: f64,
    pub scale: Colorscale,
    pub texts: Option<Vec<Vec<String>>>,
    pub text_size: f64,
    pub xgap: f64,
    pub ygap: f64,
}

// ---------------------------------------------------------------- autotype

#[allow(clippy::needless_range_loop)]
pub fn autotype_axes(ctx: &Ctx, traces: &[Trace], axes: &mut [Axis]) {
    let strict = ctx.lay_str("autotypenumbers") == Some("strict");
    for ai in 0..axes.len() {
        if !axes[ai].type_unset {
            continue;
        }
        let letter = axes[ai].letter;
        let on = |t: &Trace| if letter == 'x' { t.xa == ai } else { t.ya == ai };
        let Some(first) = traces.iter().find(|t| {
            on(t) && (t.raw.get(letter.to_string()).is_some_and(|v| Arr::from(v).is_some_and(|a| !a.is_empty()))
                || t.raw.get(format!("{letter}0")).is_some()
                || (t.kind == Kind::Box && box_without_positions(t, letter)))
        }) else {
            continue;
        };
        if first.kind == Kind::Histogram && letter == if first.horizontal { 'x' } else { 'y' } {
            axes[ai].ty = AxisType::Linear;
            continue;
        }
        if first.kind == Kind::Box && box_without_positions(first, letter) {
            let mut vals: Vec<Value> = Vec::new();
            for t in traces.iter().filter(|t| t.kind == Kind::Box && on(t)) {
                if let Some(v) = t.raw.get(format!("{letter}0")) {
                    vals.push(v.clone());
                } else if let Some(n) = t.raw.get("name") {
                    vals.push(n.clone());
                } else {
                    vals.push(Value::from("text"));
                }
            }
            let d: Vec<Datum> = vals.iter().map(datum_of).collect();
            axes[ai].ty = super::axis::auto_type(&d, strict);
            continue;
        }
        let ty = match first.raw.get(letter.to_string()).and_then(Arr::from) {
            Some(arr) if !arr.is_empty() => {
                let d = super::data_of(&arr);
                // Heatmap/box multi-category arrays (arrays of arrays) aren't supported.
                if d.iter().take(10).any(|v| matches!(v, Datum::Other)) {
                    axes[ai].ty = AxisType::Category;
                    axes[ai].type_unset = false;
                    continue;
                }
                super::axis::auto_type(&d, strict)
            }
            _ => {
                let v0 = first.raw.get(format!("{letter}0")).cloned().unwrap_or(Value::from(0));
                super::axis::auto_type(&[datum_of(&v0)], strict)
            }
        };
        axes[ai].ty = ty;
    }
}

fn datum_of(v: &Value) -> Datum<'_> {
    match v {
        Value::Number(n) => n.as_f64().map(Datum::Num).unwrap_or(Datum::Null),
        Value::String(s) => Datum::Str(s),
        Value::Bool(b) => Datum::Bool(*b),
        Value::Null => Datum::Null,
        _ => Datum::Other,
    }
}

fn box_without_positions(t: &Trace, letter: char) -> bool {
    let pos_letter = if t.horizontal { 'y' } else { 'x' };
    letter == pos_letter && t.raw.get(pos_letter.to_string()).is_none_or(|v| v.is_null())
}

// ---------------------------------------------------------------- categories

fn pos_values<'a>(t: &Trace<'a>, letter: char) -> Option<Arr<'a>> {
    t.arr(&letter.to_string())
}

#[allow(clippy::needless_range_loop)]
pub fn register_categories(traces: &[Trace], axes: &mut [Axis]) {
    for ai in 0..axes.len() {
        if axes[ai].ty != AxisType::Category {
            continue;
        }
        // categoryarray first when ordering by array.
        if axes[ai].st.categoryorder == "array" {
            for c in axes[ai].st.categoryarray.clone() {
                axes[ai].add_cat(c);
            }
        }
        let letter = axes[ai].letter;
        for t in traces {
            let on = if letter == 'x' { t.xa == ai } else { t.ya == ai };
            if !on {
                continue;
            }
            if t.kind == Kind::Box && box_without_positions(t, letter) {
                let label = match t.raw.get(format!("{letter}0")) {
                    Some(v) => util::label_of(v),
                    None => t.name.clone(),
                };
                axes[ai].add_cat(label);
                continue;
            }
            if let Some(arr) = pos_values(t, letter) {
                let a = &mut axes[ai];
                for i in 0..arr.len().min(1_000_000) {
                    let d = arr.get(i);
                    if matches!(d, Datum::Null | Datum::Other) {
                        continue;
                    }
                    a.add_cat(d.label());
                }
            }
        }
    }
}

pub fn order_categories(a: &mut Axis) {
    if a.ty != AxisType::Category {
        return;
    }
    let order = a.st.categoryorder.clone();
    let mut cats = a.cats.clone();
    match order.as_str() {
        "category ascending" | "category descending" => {
            cats.sort_by(|x, y| match (util::parse_num(x), util::parse_num(y)) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                _ => x.cmp(y),
            });
            if order.ends_with("descending") {
                cats.reverse();
            }
        }
        _ => return,
    }
    a.cats = cats;
    a.cat_index = a.cats.iter().enumerate().map(|(i, c)| (c.clone(), i)).collect();
}

// ---------------------------------------------------------------- calc

fn conv(axis: &Axis, arr: &Option<Arr>, n: usize, v0: f64, dv: f64) -> Vec<f64> {
    match arr {
        Some(a) => (0..n).map(|i| axis.d2l(a.get(i)).unwrap_or(f64::NAN)).collect(),
        None => (0..n)
            .map(|i| {
                let v = v0 + i as f64 * dv;
                if axis.ty == AxisType::Log { if v > 0. { v.log10() } else { f64::NAN } } else { v }
            })
            .collect(),
    }
}

fn start_step(t: &Trace, axis: &Axis, letter: char) -> (f64, f64) {
    let v0 = t.get(&format!("{letter}0")).and_then(|v| match axis.ty {
        AxisType::Category => axis.r2l(v).or_else(|| util::num(v)),
        _ => axis.d2l_value(v),
    });
    let dv = t.num(&format!("d{letter}")).unwrap_or(1.);
    (v0.unwrap_or(0.), dv)
}

/// Resolves a colorscale attribute value.
fn parse_scale(v: Option<&Value>) -> Option<Colorscale> {
    match v? {
        Value::String(s) => {
            let (name, rev) = match s.strip_suffix("_r") {
                Some(n) => (n, true),
                None => (s.as_str(), false),
            };
            Colorscale::plotly_named(name)
                .or_else(|| Colorscale::vega_named(name))
                .map(|c| if rev { c.reversed() } else { c })
        }
        Value::Array(stops) => Colorscale::from_stops(
            stops
                .iter()
                .filter_map(|s| {
                    let a = s.as_array()?;
                    Some((util::num_loose(a.first()?)?, Rgba::parse(a.get(1)?.as_str()?)?))
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Colorscale for a trace's numeric colors (prefix "" for heatmaps, "marker." for markers).
pub struct ColorMap {
    pub scale: Colorscale,
    pub cmin: f64,
    pub cmax: f64,
    pub showscale: bool,
    pub title: String,
    pub axis: Option<String>,
}

fn color_map(ctx: &Ctx, t: &Trace, prefix: &str, data: &[f64], heatmap: bool) -> ColorMap {
    let coloraxis = t.str(&format!("{prefix}coloraxis")).map(str::to_string);
    let ca = coloraxis.as_deref().and_then(|c| ctx.lay(c));
    let from = |k: &str| -> Option<&Value> {
        match ca {
            Some(c) => util::path(c, k),
            None => t.get(&format!("{prefix}{k}")),
        }
    };
    let (dmin, dmax) = util::extent(data.iter().copied()).unwrap_or((0., 1.));
    let (cmin_k, cmax_k) = if heatmap && ca.is_none() { ("zmin", "zmax") } else { ("cmin", "cmax") };
    let cmid = from(if heatmap && ca.is_none() { "zmid" } else { "cmid" }).and_then(util::num);
    let mut cmin = from(cmin_k).and_then(util::num).unwrap_or(dmin);
    let mut cmax = from(cmax_k).and_then(util::num).unwrap_or(dmax);
    if let Some(mid) = cmid
        && from(cmin_k).is_none()
        && from(cmax_k).is_none()
    {
        let half = (dmax - mid).abs().max((mid - dmin).abs());
        cmin = mid - half;
        cmax = mid + half;
    }
    if cmin == cmax {
        cmin -= 0.5;
        cmax += 0.5;
    }
    let explicit = from("colorscale");
    let auto_default = !heatmap && explicit.is_none();
    let auto = from("autocolorscale").and_then(|v| v.as_bool()).unwrap_or(auto_default);
    let mut scale = parse_scale(explicit);
    if scale.is_none() && heatmap {
        scale = parse_scale(t.tmpl.and_then(|tm| tm.get("colorscale")));
    }
    if scale.is_none() || (auto && explicit.is_none()) {
        let key = if cmin * cmax < 0. {
            "colorscale.diverging"
        } else if cmin >= 0. {
            "colorscale.sequential"
        } else {
            "colorscale.sequentialminus"
        };
        let layout_scale = if auto || scale.is_none() { parse_scale(ctx.lay(key)) } else { None };
        scale = scale.or(layout_scale).or_else(|| {
            Colorscale::plotly_named(if heatmap || cmin * cmax < 0. {
                "RdBu"
            } else if cmin >= 0. {
                "Reds"
            } else {
                "Blues"
            })
        });
    }
    let mut scale = scale.unwrap_or_else(|| Colorscale::plotly_named("RdBu").expect("builtin"));
    if from("reversescale").and_then(|v| v.as_bool()).unwrap_or(false) {
        scale = scale.reversed();
    }
    let showscale = from("showscale").and_then(|v| v.as_bool()).unwrap_or(heatmap || ca.is_some());
    let title = match from("colorbar.title") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => util::path(v, "text").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        None => String::new(),
    };
    ColorMap {
        scale,
        cmin,
        cmax,
        showscale,
        title,
        axis: coloraxis,
    }
}

fn colors_from(ctx: &Ctx, t: &Trace, attr: &str, default: Rgba, prefix: &str) -> Each<Rgba> {
    match t.get(attr) {
        Some(Value::String(s)) => Each::One(Rgba::parse(s).unwrap_or(default)),
        Some(v @ Value::Array(_)) | Some(v @ Value::Object(_)) => {
            let Some(arr) = Arr::from(v) else { return Each::One(default) };
            if arr.is_empty() {
                return Each::One(default);
            }
            let numeric = (0..arr.len().min(50)).all(|i| matches!(arr.get(i), Datum::Num(_) | Datum::Null));
            if numeric {
                let vals: Vec<f64> = (0..arr.len()).map(|i| arr.num(i).unwrap_or(f64::NAN)).collect();
                let cm = color_map(ctx, t, prefix, &vals, false);
                let span = cm.cmax - cm.cmin;
                Each::Many(
                    vals.iter()
                        .map(|&v| if v.is_finite() { cm.scale.at((v - cm.cmin) / span) } else { Rgba::hex_rgb(0x444444) })
                        .collect(),
                )
            } else {
                Each::Many(
                    (0..arr.len())
                        .map(|i| match arr.get(i) {
                            Datum::Str(s) => Rgba::parse(s).unwrap_or(default),
                            _ => default,
                        })
                        .collect(),
                )
            }
        }
        _ => Each::One(default),
    }
}

fn nums_each(t: &Trace, attr: &str, default: f64) -> Each<f64> {
    match t.get(attr) {
        Some(v @ (Value::Array(_) | Value::Object(_))) => match Arr::from(v) {
            Some(a) if !a.is_empty() => Each::Many((0..a.len()).map(|i| a.num(i).unwrap_or(f64::NAN)).collect()),
            _ => Each::One(default),
        },
        Some(v) => Each::One(util::num(v).unwrap_or(default)),
        None => Each::One(default),
    }
}

fn marker(ctx: &Ctx, t: &Trace, default_color: Rgba, n: usize) -> Marker {
    let size = nums_each(t, "marker.size", 6.);
    let array_size = size.is_many();
    let sizeref = t.num("marker.sizeref").filter(|s| *s > 0.).unwrap_or(1.);
    let area = t.str("marker.sizemode") == Some("area");
    let sizemin = t.num("marker.sizemin").unwrap_or(0.);
    let (radius, ppad) = match &size {
        Each::One(s) => {
            let s = if s.is_finite() { s.max(0.) } else { 6. };
            (Each::One(s / 2.), PadSpec::Const((s / 1.6).max(3.)))
        }
        Each::Many(v) => {
            let base = |x: f64| if area { (x / sizeref).sqrt() } else { x / sizeref };
            let radius: Vec<f64> = v
                .iter()
                .take(n)
                .map(|&x| {
                    let b = base(x / 2.);
                    if b.is_finite() && b > 0. { b.max(sizemin) } else { 0. }
                })
                .collect();
            let sr2 = 1.6 * sizeref;
            let pad: Vec<f64> = v
                .iter()
                .take(n)
                .map(|&x| {
                    let x = if x.is_finite() { x.max(0.) } else { 0. };
                    (if area { (x / sr2).sqrt() } else { x / sr2 }).max(3.)
                })
                .collect();
            (Each::Many(radius), PadSpec::Each(pad))
        }
    };
    let symbol = match t.get("marker.symbol") {
        Some(Value::Array(a)) => Each::Many(a.iter().map(Symbol::parse).collect()),
        Some(v) => Each::One(Symbol::parse(v)),
        None => Each::One(Symbol::CIRCLE),
    };
    let opacity = nums_each(t, "marker.opacity", if array_size { 0.7 } else { 1. });
    let line_width = t.num("marker.line.width").unwrap_or(if array_size { 1. } else { 0. });
    let line_default = if array_size { Rgba::WHITE } else { Rgba::hex_rgb(0x444444) };
    let line_color = colors_from(ctx, t, "marker.line.color", line_default, "marker.line.");
    Marker {
        color: colors_from(ctx, t, "marker.color", default_color, "marker."),
        radius,
        ppad,
        symbol,
        opacity,
        line_color,
        line_width,
        array_size,
    }
}

fn err_bars(t: &Trace, letter: char, vals: &[f64], color: Rgba) -> Option<ErrBars> {
    let key = format!("error_{letter}");
    let e = t.raw.get(&key)?;
    if e.get("visible").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or(if e.get("array").is_some() { "data" } else { "percent" });
    let symmetric = e.get("symmetric").and_then(|v| v.as_bool()).unwrap_or(e.get("arrayminus").is_none() && e.get("valueminus").is_none());
    let n = vals.len();
    let arr = |k: &str| -> Option<Vec<f64>> {
        let a = Arr::from(e.get(k)?)?;
        Some((0..n).map(|i| a.num(i).unwrap_or(f64::NAN)).collect())
    };
    let value = e.get("value").and_then(util::num).unwrap_or(10.);
    let valueminus = e.get("valueminus").and_then(util::num).unwrap_or(value);
    let calc = |v: f64, val: f64| -> f64 {
        match ty {
            "percent" => (v * val / 100.).abs(),
            "constant" => val,
            "sqrt" => v.abs().sqrt(),
            _ => f64::NAN,
        }
    };
    let (plus, minus) = match ty {
        "data" => {
            let p = arr("array")?;
            let m = if symmetric { p.clone() } else { arr("arrayminus").unwrap_or_else(|| vec![f64::NAN; n]) };
            (p, m)
        }
        _ => {
            let p: Vec<f64> = vals.iter().map(|&v| calc(v, value)).collect();
            let m: Vec<f64> = if symmetric { p.clone() } else { vals.iter().map(|&v| calc(v, valueminus)).collect() };
            (p, m)
        }
    };
    let color = e.get("color").and_then(|v| v.as_str()).and_then(Rgba::parse).unwrap_or(color);
    Some(ErrBars {
        plus,
        minus,
        color,
        thickness: e.get("thickness").and_then(util::num).unwrap_or(2.),
        width: e.get("width").and_then(util::num).unwrap_or(4.),
    })
}

pub fn calc(ctx: &Ctx, t: &Trace, axes: &mut [Axis], notes: &mut Vec<String>) -> Option<Calc> {
    match t.kind {
        Kind::Scatter => calc_scatter(ctx, t, axes).map(|c| Calc::Scatter(Box::new(c))),
        Kind::Bar | Kind::Histogram => calc_bar(ctx, t, axes).map(|c| Calc::Bars(Box::new(c))),
        Kind::Box => calc_box(ctx, t, axes, notes).map(|c| Calc::Box(Box::new(c))),
        Kind::Heatmap => calc_heatmap(ctx, t, axes).map(|c| Calc::Heatmap(Box::new(c))),
        Kind::Histogram2d => calc_hist2d(ctx, t, axes).map(|c| Calc::Heatmap(Box::new(c))),
        Kind::Image => calc_image(t, axes).map(|c| Calc::Image(Box::new(c))),
        Kind::Candlestick => calc_candles(t, axes, false).map(|c| Calc::Box(Box::new(c))),
        Kind::Ohlc => calc_candles(t, axes, true).map(|c| Calc::Box(Box::new(c))),
        Kind::Pie => super::pie::calc(ctx, t).map(|c| Calc::Pie(Box::new(c))),
    }
}

fn calc_scatter(ctx: &Ctx, t: &Trace, axes: &mut [Axis]) -> Option<ScatterCalc> {
    let xarr = t.arr("x");
    let yarr = t.arr("y");
    let n = match (&xarr, &yarr) {
        (Some(x), Some(y)) => x.len().min(y.len()),
        (Some(x), None) => x.len(),
        (None, Some(y)) => y.len(),
        (None, None) => 0,
    };
    let (x0, dx) = start_step(t, &axes[t.xa], 'x');
    let (y0, dy) = start_step(t, &axes[t.ya], 'y');
    let x = conv(&axes[t.xa], &xarr, n, x0, dx);
    let y = conv(&axes[t.ya], &yarr, n, y0, dy);
    let mode = t.str("mode").map(str::to_string).unwrap_or_else(|| if n < 20 { "lines+markers".into() } else { "lines".into() });
    let stackgroup = t.get("stackgroup").map(util::label_of).filter(|s| !s.is_empty());
    let lines = mode.contains("lines");
    let markers = mode.contains("markers");
    let has_text = mode.contains("text");
    let marker_scalar = t.color_attr("marker.color");
    let line_color = t.color_attr("line.color").or(marker_scalar).unwrap_or(t.color);
    let line_width = t.num("line.width").unwrap_or(2.).max(0.);
    let dash = t.str("line.dash").and_then(|d| super::dash_array(d, line_width));
    let shape = match t.str("line.shape") {
        Some("spline") => LineShape::Spline,
        Some("hv") => LineShape::Hv,
        Some("vh") => LineShape::Vh,
        Some("hvh") => LineShape::Hvh,
        Some("vhv") => LineShape::Vhv,
        _ => LineShape::Linear,
    };
    let mk = marker(ctx, t, if lines { line_color } else { t.color }, n);
    let fill = match t.str("fill").unwrap_or(if stackgroup.is_some() { "tonexty" } else { "none" }) {
        "tozeroy" => Fill::ToZeroY,
        "tozerox" => Fill::ToZeroX,
        "tonexty" => Fill::ToNextY,
        "tonextx" => Fill::ToNextX,
        "toself" => Fill::ToSelf,
        "tonext" => Fill::ToNext,
        _ => Fill::None,
    };
    let inherit = match &mk.color {
        Each::One(c) if t.get("marker.color").is_some() => Some(*c),
        _ => None,
    };
    let fill_base = t.color_attr("line.color").or(inherit).unwrap_or(t.color);
    let fillcolor = t.color_attr("fillcolor").unwrap_or(fill_base.scale_alpha(0.5));
    let texts = text_array(t, n);
    let err_y = err_bars(t, 'y', &y_values_raw(&yarr, n, y0, dy), mk.color.first());
    let err_x = err_bars(t, 'x', &y_values_raw(&xarr, n, x0, dx), mk.color.first());
    let text_size = t.num("textfont.size").unwrap_or(ctx.theme.font_size);
    let text_color = t.color_attr("textfont.color").unwrap_or(ctx.theme.font_color);
    Some(ScatterCalc {
        x,
        y,
        lines,
        markers,
        text: has_text,
        line_color,
        line_width,
        dash,
        shape,
        connectgaps: t.get("connectgaps").and_then(|v| v.as_bool()).unwrap_or(false),
        marker: mk,
        fill,
        fillcolor,
        stackgroup,
        groupnorm: t.str("groupnorm").unwrap_or("").to_string(),
        base: None,
        next_to: None,
        texts,
        textpos: t.str("textposition").unwrap_or("middle center").to_string(),
        text_size,
        text_color,
        err_x,
        err_y,
    })
}

/// Raw numeric values (for percent error bars).
fn y_values_raw(arr: &Option<Arr>, n: usize, v0: f64, dv: f64) -> Vec<f64> {
    match arr {
        Some(a) => (0..n).map(|i| a.num(i).unwrap_or(f64::NAN)).collect(),
        None => (0..n).map(|i| v0 + i as f64 * dv).collect(),
    }
}

fn text_array(t: &Trace, n: usize) -> Vec<String> {
    match t.get("text") {
        Some(Value::String(s)) => vec![s.clone(); n.min(100_000)],
        Some(v) => match Arr::from(v) {
            Some(a) => (0..a.len().min(n)).map(|i| a.get(i).label()).collect(),
            None => Vec::new(),
        },
        None => Vec::new(),
    }
}

fn calc_bar(ctx: &Ctx, t: &Trace, axes: &mut [Axis]) -> Option<BarCalc> {
    let (pl, sl) = if t.horizontal { ('y', 'x') } else { ('x', 'y') };
    let (pa_i, sa_i) = if t.horizontal { (t.ya, t.xa) } else { (t.xa, t.ya) };
    let parr = t.arr(&pl.to_string());
    let sarr = t.arr(&sl.to_string());
    let hist = t.kind == Kind::Histogram;
    let color = colors_from(ctx, t, "marker.color", t.color, "marker.");
    let line_color = colors_from(ctx, t, "marker.line.color", Rgba::hex_rgb(0x444444), "marker.line.");
    let line_width = t.num("marker.line.width").unwrap_or(0.);
    let opacity = t.num("marker.opacity").unwrap_or(1.).clamp(0., 1.);
    let textposition = t.str("textposition").unwrap_or(if hist { "none" } else { "auto" }).to_string();
    let base_bar = BarCalc {
        pos: Vec::new(),
        val: Vec::new(),
        base: Vec::new(),
        explicit_base: false,
        width: None,
        offset: None,
        color,
        line_color,
        line_width,
        opacity,
        texts: Vec::new(),
        textposition,
        textfont_size: t.num("textfont.size").unwrap_or(ctx.theme.font_size),
        textfont_color: t.color_attr("textfont.color"),
        hist: None,
        offsetgroup: t.get("offsetgroup").map(util::label_of).unwrap_or_default(),
        p0: Vec::new(),
        p1: Vec::new(),
        s0: Vec::new(),
        s1: Vec::new(),
    };
    if hist {
        let pa = &axes[pa_i];
        let data_arr = parr?;
        let data: Vec<f64> = (0..data_arr.len()).map(|i| pa.d2l(data_arr.get(i)).unwrap_or(f64::NAN)).collect();
        let func = t.str("histfunc").unwrap_or("count").to_string();
        let weights = sarr.map(|a| (0..data.len()).map(|i| a.num(i).unwrap_or(f64::NAN)).collect());
        let bins = |k: &str| t.raw.get(format!("{pl}bins")).and_then(|b| b.get(k)).filter(|v| !v.is_null()).cloned();
        let hi = HistInput {
            data,
            weights,
            func,
            norm: t.str("histnorm").unwrap_or("").to_string(),
            nbins: t.num(&format!("nbins{pl}")).filter(|n| *n > 0.),
            start: bins("start"),
            end: bins("end"),
            size: bins("size"),
            cumulative: t.raw.get("cumulative").and_then(|c| c.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(false),
            bingroup: t.get("bingroup").map(util::label_of).unwrap_or_default(),
            categorical: pa.ty == AxisType::Category,
        };
        return Some(BarCalc { hist: Some(hi), ..base_bar });
    }
    let n = match (&parr, &sarr) {
        (Some(p), Some(s)) => p.len().min(s.len()),
        (None, Some(s)) => s.len(),
        (Some(p), None) => p.len(),
        _ => 0,
    };
    let (p0, dp) = start_step(t, &axes[pa_i], pl);
    let pos = conv(&axes[pa_i], &parr, n, p0, dp);
    let sa = &axes[sa_i];
    let val: Vec<f64> = match &sarr {
        Some(a) => (0..n)
            .map(|i| match sa.ty {
                AxisType::Log => a.num(i).unwrap_or(f64::NAN),
                AxisType::Date => a.num(i).or_else(|| sa.d2l(a.get(i))).unwrap_or(f64::NAN),
                _ => sa.d2l(a.get(i)).unwrap_or(f64::NAN),
            })
            .collect(),
        None => vec![f64::NAN; n],
    };
    let base_v = t.get("base");
    let explicit_base = base_v.is_some();
    let base: Vec<f64> = match base_v {
        Some(v @ (Value::Array(_) | Value::Object(_))) => match Arr::from(v) {
            Some(a) => (0..n).map(|i| sa.d2l(a.get(i)).unwrap_or(0.)).collect(),
            None => vec![0.; n],
        },
        Some(v) => vec![sa.d2l_value(v).unwrap_or(0.); n],
        None => vec![0.; n],
    };
    let width = match t.get("width") {
        Some(v @ Value::Array(_)) => Some(Each::Many(Arr::from(v).map(|a| a.nums().into_iter().map(|x| x.unwrap_or(f64::NAN)).collect()).unwrap_or_default())),
        Some(v) => util::num(v).map(Each::One),
        None => None,
    };
    let offset = match t.get("offset") {
        Some(v @ Value::Array(_)) => Some(Each::Many(Arr::from(v).map(|a| a.nums().into_iter().map(|x| x.unwrap_or(0.)).collect()).unwrap_or_default())),
        Some(v) => util::num(v).map(Each::One),
        None => None,
    };
    let texts = bar_texts(t, &val, &pos, &axes[pa_i], &axes[sa_i], n);
    Some(BarCalc {
        pos,
        val,
        base,
        explicit_base,
        width,
        offset,
        texts,
        ..base_bar
    })
}

/// Text shown on bars: `text` or `texttemplate` (`%{y}`, `%{value:.2f}`...).
fn bar_texts(t: &Trace, val: &[f64], pos: &[f64], pa: &Axis, sa: &Axis, n: usize) -> Vec<String> {
    let tt = t.str("texttemplate");
    let raw = text_array(t, n);
    let Some(tt) = tt else { return raw };
    let (pl, sl) = if t.horizontal { ("y", "x") } else { ("x", "y") };
    (0..n)
        .map(|i| {
            template(tt, |name, fmt| {
                let v = match name {
                    n if n == sl || n == "value" => Some(val[i]),
                    n if n == pl || n == "label" => {
                        if pa.ty == AxisType::Category {
                            return pa.cats.get(pos[i] as usize).cloned();
                        }
                        Some(pos[i])
                    }
                    "text" => return raw.get(i).cloned(),
                    _ => None,
                }?;
                Some(fmt_value(v, fmt, sa))
            })
        })
        .collect()
}

fn fmt_value(v: f64, fmt: Option<&str>, axis: &Axis) -> String {
    match fmt {
        Some(f) => format::minus(format::d3(f, v)),
        None => {
            if axis.ty == AxisType::Date {
                time::strftime("%b %-d, %Y", v)
            } else {
                format::plotly_hover_num(v, &axis.exp)
            }
        }
    }
}

/// Expands `%{name}` / `%{name:fmt}` placeholders.
pub fn template(tt: &str, mut f: impl FnMut(&str, Option<&str>) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = tt;
    while let Some(i) = rest.find("%{") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let Some(end) = tail.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let inner = &tail[..end];
        let (name, fmt) = match inner.split_once(':') {
            Some((n, f)) => (n.trim(), Some(f)),
            None => (inner.trim(), None),
        };
        if let Some(s) = f(name, fmt) {
            out.push_str(&s);
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    text::plain(&out)
}

fn calc_box(_ctx: &Ctx, t: &Trace, axes: &mut [Axis], notes: &mut Vec<String>) -> Option<BoxCalc> {
    let (pl, vl) = if t.horizontal { ('y', 'x') } else { ('x', 'y') };
    let (pa_i, va_i) = if t.horizontal { (t.ya, t.xa) } else { (t.xa, t.ya) };
    if t.raw.get("q1").is_some() || t.raw.get("median").is_some() {
        // Precomputed statistics.
        push_note(notes, "precomputed box statistics drawn without points");
    }
    let varr = t.arr(&vl.to_string());
    let parr = t.arr(&pl.to_string());
    let va = &axes[va_i];
    let pa = &axes[pa_i];
    let vals: Vec<f64> = match &varr {
        Some(a) => (0..a.len()).map(|i| va.d2l(a.get(i)).unwrap_or(f64::NAN)).collect(),
        None => Vec::new(),
    };
    let single_pos = || -> f64 {
        if let Some(v) = t.raw.get(format!("{pl}0")) {
            if pa.ty == AxisType::Category {
                return pa.cat_index.get(&util::label_of(v)).map(|&i| i as f64).unwrap_or(0.);
            }
            return pa.d2l_value(v).unwrap_or(0.);
        }
        if pa.ty == AxisType::Category {
            return pa.cat_index.get(&t.name).map(|&i| i as f64).unwrap_or(0.);
        }
        util::parse_num(&t.name).unwrap_or(t.idx as f64)
    };
    let mut groups: Vec<(f64, Vec<f64>)> = Vec::new();
    match &parr {
        Some(p) => {
            let mut index: HashMap<u64, usize> = HashMap::new();
            for (i, &v) in vals.iter().enumerate() {
                let pv = pa.d2l(p.get(i)).unwrap_or(f64::NAN);
                if !pv.is_finite() || !v.is_finite() {
                    continue;
                }
                let k = *index.entry(pv.to_bits()).or_insert_with(|| {
                    groups.push((pv, Vec::new()));
                    groups.len() - 1
                });
                groups[k].1.push(v);
            }
        }
        None => groups.push((single_pos(), vals.iter().copied().filter(|v| v.is_finite()).collect())),
    }
    let mut boxes = Vec::new();
    for (pos, mut v) in groups {
        if v.is_empty() {
            continue;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let interp = |p: f64| -> f64 {
            // plotly's Lib.interp: position p*n - 0.5
            let n = v.len() as f64;
            let k = p * n - 0.5;
            if k < 0. {
                return v[0];
            }
            if k > n - 1. {
                return v[v.len() - 1];
            }
            let frac = k.fract();
            frac * v[k.ceil() as usize] + (1. - frac) * v[k.floor() as usize]
        };
        let method = t.str("quartilemethod").unwrap_or("linear");
        let (q1, med, q3) = match method {
            "exclusive" | "inclusive" if v.len() > 1 => {
                let n = v.len();
                let med = interp(0.5);
                let (lo, hi) = if n % 2 == 1 {
                    if method == "exclusive" { (&v[..n / 2], &v[n / 2 + 1..]) } else { (&v[..=n / 2], &v[n / 2..]) }
                } else {
                    (&v[..n / 2], &v[n / 2..])
                };
                let m = |s: &[f64]| {
                    let k = s.len();
                    if k == 0 {
                        med
                    } else if k % 2 == 1 {
                        s[k / 2]
                    } else {
                        (s[k / 2 - 1] + s[k / 2]) / 2.
                    }
                };
                (m(lo), med, m(hi))
            }
            _ => (interp(0.25), interp(0.5), interp(0.75)),
        };
        let iqr = q3 - q1;
        let lo_fence = q1 - 1.5 * iqr;
        let hi_fence = q3 + 1.5 * iqr;
        let lf = v.iter().copied().find(|&x| x >= lo_fence).unwrap_or(q1).min(q1);
        let uf = v.iter().rev().copied().find(|&x| x <= hi_fence).unwrap_or(q3).max(q3);
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let outliers: Vec<f64> = v.iter().copied().filter(|&x| x < lf || x > uf).collect();
        boxes.push(BoxStat {
            pos,
            q1,
            med,
            q3,
            lf,
            uf,
            mean,
            sd,
            points: v,
            outliers,
        });
    }
    let line_color = t.color_attr("line.color").or_else(|| t.color_attr("marker.color")).unwrap_or(t.color);
    let line_width = t.num("line.width").unwrap_or(2.);
    let fillcolor = t.color_attr("fillcolor").unwrap_or(line_color.scale_alpha(0.5));
    let marker_color = t.color_attr("marker.color").unwrap_or(line_color);
    let has_outlier_style = t.get("marker.outliercolor").is_some() || t.get("marker.line.outliercolor").is_some();
    let boxpoints = match t.get("boxpoints") {
        Some(Value::Bool(false)) => "false".to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => if has_outlier_style { "suspectedoutliers".into() } else { "outliers".into() },
    };
    let boxmean = match t.get("boxmean") {
        Some(Value::Bool(true)) => "true".to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    Some(BoxCalc {
        boxes,
        line_color,
        line_width,
        fillcolor,
        marker_color,
        marker_radius: t.num("marker.size").unwrap_or(6.) / 2.,
        boxpoints: boxpoints.clone(),
        boxmean,
        whiskerwidth: t.num("whiskerwidth").unwrap_or(0.5),
        jitter: t.num("jitter").unwrap_or(if boxpoints == "all" { 0.3 } else { 0. }),
        pointpos: t.num("pointpos").unwrap_or(if boxpoints == "all" { -1.5 } else { 0. }),
        offsetgroup: t.get("offsetgroup").map(util::label_of).unwrap_or_default(),
        width: 0.,
        offset: 0.,
        colors: None,
        width_frac: None,
        ohlc: false,
    })
}

/// candlestick / ohlc: one box per bar (box = open..close, median = close,
/// whiskers = low..high), colored by direction.
fn calc_candles(t: &Trace, axes: &mut [Axis], ohlc: bool) -> Option<BoxCalc> {
    let pa = &axes[t.xa];
    let get = |k: &str| t.arr(k).map(|a| a.nums());
    let (open, high, low, close) = (get("open")?, get("high")?, get("low")?, get("close")?);
    let n = open.len().min(high.len()).min(low.len()).min(close.len());
    let xarr = t.arr("x");
    let (x0, dx) = start_step(t, pa, 'x');
    let xs = conv(pa, &xarr, n, x0, dx);
    let color = |k: &str, d: u32| t.color_attr(&format!("{k}.line.color")).unwrap_or(Rgba::hex_rgb(d));
    let inc = color("increasing", 0x3d9970);
    let dec = color("decreasing", 0xff4136);
    let inc_fill = t.color_attr("increasing.fillcolor").unwrap_or(inc.scale_alpha(0.5));
    let dec_fill = t.color_attr("decreasing.fillcolor").unwrap_or(dec.scale_alpha(0.5));
    let mut boxes = Vec::new();
    let mut colors = Vec::new();
    for i in 0..n {
        let (Some(o), Some(h), Some(l), Some(c)) = (open[i], high[i], low[i], close[i]) else { continue };
        if !xs[i].is_finite() {
            continue;
        }
        let up = c >= o;
        boxes.push(BoxStat {
            pos: xs[i],
            q1: o.min(c),
            med: c,
            q3: o.max(c),
            lf: l.min(o.min(c)),
            uf: h.max(o.max(c)),
            mean: o,
            sd: 0.,
            points: Vec::new(),
            outliers: Vec::new(),
        });
        colors.push(if up { (inc, inc_fill) } else { (dec, dec_fill) });
    }
    let tickwidth = t.num("tickwidth").unwrap_or(0.3);
    Some(BoxCalc {
        boxes,
        line_color: inc,
        line_width: t.num("line.width").unwrap_or(2.),
        fillcolor: inc_fill,
        marker_color: inc,
        marker_radius: 0.,
        boxpoints: "false".into(),
        boxmean: String::new(),
        whiskerwidth: t.num("whiskerwidth").unwrap_or(0.),
        jitter: 0.,
        pointpos: 0.,
        offsetgroup: String::new(),
        width: 0.,
        offset: 0.,
        colors: Some(colors),
        // plotly: boxes at (1 - boxgap)(1 - boxgroupgap) = 0.49; OHLC ticks
        // reach tickwidth of the spacing to each side.
        width_frac: Some(if ohlc { 2. * tickwidth } else { 0.49 }),
        ohlc,
    })
}

/// Cell edges from centers (or pass-through edges), plotly `makeBoundArray`.
fn bounds(centers: &[f64], n: usize) -> Vec<f64> {
    if centers.len() == n + 1 {
        return centers.to_vec();
    }
    if centers.len() < n || n == 0 {
        return (0..=n).map(|i| i as f64 - 0.5).collect();
    }
    let c = &centers[..n];
    if n == 1 {
        return vec![c[0] - 0.5, c[0] + 0.5];
    }
    let mut e = Vec::with_capacity(n + 1);
    e.push(c[0] - (c[1] - c[0]) / 2.);
    for i in 0..n - 1 {
        e.push((c[i] + c[i + 1]) / 2.);
    }
    e.push(c[n - 1] + (c[n - 1] - c[n - 2]) / 2.);
    e
}

fn calc_heatmap(ctx: &Ctx, t: &Trace, axes: &mut [Axis]) -> Option<HeatCalc> {
    let mut z = util::matrix(t.raw.get("z")?)?;
    if t.get("transpose").and_then(|v| v.as_bool()) == Some(true) {
        let cols = z.iter().map(|r| r.len()).max().unwrap_or(0);
        z = (0..cols).map(|c| z.iter().map(|r| r.get(c).copied().flatten()).collect()).collect();
    }
    let rows = z.len();
    let cols = z.iter().map(|r| r.len()).max().unwrap_or(0);
    if rows == 0 || cols == 0 || rows.saturating_mul(cols) > 4_000_000 {
        return None;
    }
    let xa = &axes[t.xa];
    let ya = &axes[t.ya];
    let centers = |axis: &Axis, letter: char, n: usize| -> Vec<f64> {
        match t.arr(&letter.to_string()) {
            Some(a) => (0..a.len().min(n + 1)).map(|i| axis.d2l(a.get(i)).unwrap_or(i as f64)).collect(),
            None => {
                let (v0, dv) = start_step(t, axis, letter);
                (0..n).map(|i| v0 + i as f64 * dv).collect()
            }
        }
    };
    let xe = bounds(&centers(xa, 'x', cols), cols);
    let ye = bounds(&centers(ya, 'y', rows), rows);
    let flat: Vec<f64> = z.iter().flatten().filter_map(|v| *v).collect();
    let cm = color_map(ctx, t, "", &flat, true);
    let texts = t.str("texttemplate").map(|tt| z.iter()
                .map(|row| {
                    row.iter()
                        .map(|v| match v {
                            Some(v) => template(tt, |name, fmt| match name {
                                "z" | "value" => Some(match fmt {
                                    Some(f) => format::minus(format::d3(f, *v)),
                                    None => format::plotly_hover_num(*v, &ExpFormat::default()),
                                }),
                                _ => None,
                            }),
                            None => String::new(),
                        })
                        .collect()
                })
                .collect());
    Some(HeatCalc {
        xe,
        ye,
        z,
        zmin: cm.cmin,
        zmax: cm.cmax,
        scale: cm.scale,
        texts,
        text_size: t.num("textfont.size").unwrap_or(ctx.theme.font_size),
        xgap: t.num("xgap").unwrap_or(0.),
        ygap: t.num("ygap").unwrap_or(0.),
    })
}

/// histogram2d (px.density_heatmap): 2-D binning drawn as a heatmap.
fn calc_hist2d(ctx: &Ctx, t: &Trace, axes: &mut [Axis]) -> Option<HeatCalc> {
    let xarr = t.arr("x")?;
    let yarr = t.arr("y")?;
    let n = xarr.len().min(yarr.len());
    let (xa, ya) = (&axes[t.xa], &axes[t.ya]);
    let xs: Vec<f64> = (0..n).map(|i| xa.d2l(xarr.get(i)).unwrap_or(f64::NAN)).collect();
    let ys: Vec<f64> = (0..n).map(|i| ya.d2l(yarr.get(i)).unwrap_or(f64::NAN)).collect();
    let spec = |axis: &Axis, vals: &[f64], l: char| {
        let finite: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
        let bins = |k: &str| t.raw.get(format!("{l}bins")).and_then(|b| b.get(k)).filter(|v| !v.is_null()).cloned();
        auto_bin(&finite, axis, t.num(&format!("nbins{l}")).filter(|v| *v > 0.), bins("start").as_ref(), bins("end").as_ref(), bins("size").as_ref(), true)
    };
    let (bx, by) = (spec(xa, &xs, 'x'), spec(ya, &ys, 'y'));
    let (xe, ye) = (bin_edges(&bx), bin_edges(&by));
    let (nx, ny) = (xe.len() - 1, ye.len() - 1);
    if nx * ny > 4_000_000 {
        return None;
    }
    let index = |edges: &[f64], v: f64| -> Option<usize> {
        if !v.is_finite() || v < edges[0] || v >= edges[edges.len() - 1] {
            return None;
        }
        Some(edges.partition_point(|&e| e <= v).saturating_sub(1).min(edges.len() - 2))
    };
    let func = t.str("histfunc").unwrap_or("count");
    let zarr = t.arr("z");
    let mut acc = vec![vec![0f64; nx]; ny];
    let mut cnt = vec![vec![0usize; nx]; ny];
    let mut total = 0.;
    for i in 0..n {
        let (Some(c), Some(r)) = (index(&xe, xs[i]), index(&ye, ys[i])) else { continue };
        let w = zarr.as_ref().and_then(|z| z.num(i));
        let v = match func {
            "count" => 1.,
            _ => match w {
                Some(w) => w,
                None => continue,
            },
        };
        let cell = &mut acc[r][c];
        match func {
            "min" => *cell = if cnt[r][c] == 0 { v } else { cell.min(v) },
            "max" => *cell = if cnt[r][c] == 0 { v } else { cell.max(v) },
            _ => *cell += v,
        }
        cnt[r][c] += 1;
        total += v;
    }
    let areas = |r: usize, c: usize| (xe[c + 1] - xe[c]) * (ye[r + 1] - ye[r]);
    let norm = t.str("histnorm").unwrap_or("");
    let z: Vec<Vec<Option<f64>>> = (0..ny)
        .map(|r| {
            (0..nx)
                .map(|c| {
                    let mut v = acc[r][c];
                    if func == "avg" {
                        v = if cnt[r][c] > 0 { v / cnt[r][c] as f64 } else { return None };
                    }
                    if matches!(func, "min" | "max") && cnt[r][c] == 0 {
                        return None;
                    }
                    Some(match norm {
                        "percent" => v * 100. / total.max(1e-300),
                        "probability" => v / total.max(1e-300),
                        "density" => v / areas(r, c),
                        "probability density" => v / areas(r, c) / total.max(1e-300),
                        _ => v,
                    })
                })
                .collect()
        })
        .collect();
    let flat: Vec<f64> = z.iter().flatten().filter_map(|v| *v).collect();
    let cm = color_map(ctx, t, "", &flat, true);
    Some(HeatCalc {
        xe,
        ye,
        z,
        zmin: cm.cmin,
        zmax: cm.cmax,
        scale: cm.scale,
        texts: None,
        text_size: ctx.theme.font_size,
        xgap: t.num("xgap").unwrap_or(0.),
        ygap: t.num("ygap").unwrap_or(0.),
    })
}

/// Pixel size of a base64 PNG / JPEG data URI.
fn data_uri_size(src: &str) -> Option<(usize, usize)> {
    let b64 = src.split_once("base64,")?.1;
    let head = util::base64(b64.get(..(b64.len().min(64 * 1024) / 4) * 4)?)?;
    if head.len() > 24 && &head[1..4] == b"PNG" {
        let w = u32::from_be_bytes(head[16..20].try_into().ok()?) as usize;
        let h = u32::from_be_bytes(head[20..24].try_into().ok()?) as usize;
        return Some((w, h));
    }
    if head.len() > 4 && head[0] == 0xff && head[1] == 0xd8 {
        let mut i = 2;
        while i + 9 < head.len() {
            if head[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = head[i + 1];
            let len = u16::from_be_bytes([head[i + 2], head[i + 3]]) as usize;
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                let h = u16::from_be_bytes([head[i + 5], head[i + 6]]) as usize;
                let w = u16::from_be_bytes([head[i + 7], head[i + 8]]) as usize;
                return Some((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// image traces (px.imshow of RGB data): pixels embedded as a PNG.
fn calc_image(t: &Trace, axes: &mut [Axis]) -> Option<ImageCalc> {
    let x0 = t.num("x0").unwrap_or(0.);
    let y0 = t.num("y0").unwrap_or(0.);
    let dx = t.num("dx").filter(|d| *d != 0.).unwrap_or(1.);
    let dy = t.num("dy").filter(|d| *d != 0.).unwrap_or(1.);
    let (href, w, h) = if let Some(src) = t.str("source").filter(|s| s.starts_with("data:image/")) {
        let (w, h) = data_uri_size(src)?;
        (src.to_string(), w, h)
    } else {
        // z: rows of pixels of channels, or a typed array shaped (h, w, c).
        let z = t.raw.get("z")?;
        let (data, shape): (Vec<f64>, Vec<usize>) = match z {
            Value::Object(_) => util::typed_array(z)?,
            Value::Array(rows) => {
                let h = rows.len();
                let w = rows.first()?.as_array()?.len();
                let c = rows.first()?.as_array()?.first()?.as_array().map(|p| p.len()).unwrap_or(1);
                if h * w > 4_000_000 || c == 0 || c > 4 {
                    return None;
                }
                let mut d = Vec::with_capacity(h * w * c);
                for r in rows {
                    let r = r.as_array()?;
                    for k in 0..w {
                        match r.get(k) {
                            Some(Value::Array(p)) => (0..c).for_each(|j| d.push(p.get(j).and_then(util::num).unwrap_or(0.))),
                            Some(v) => d.extend(std::iter::repeat_n(util::num(v).unwrap_or(0.), c)),
                            None => d.extend(std::iter::repeat_n(0., c)),
                        }
                    }
                }
                (d, vec![h, w, c])
            }
            _ => return None,
        };
        let [h, w, c] = shape[..] else { return None };
        if h == 0 || w == 0 || h * w > 4_000_000 || data.len() < h * w * c || !(1..=4).contains(&c) {
            return None;
        }
        let model = t.str("colormodel").unwrap_or(if c == 4 { "rgba" } else { "rgb" });
        if model.starts_with("hsl") {
            return None;
        }
        let zmax = |j: usize| -> f64 {
            t.get("zmax").and_then(|v| v.as_array()).and_then(|a| a.get(j)).and_then(util::num).unwrap_or(if j == 3 { 1. } else { 255. })
        };
        let zmin = |j: usize| -> f64 { t.get("zmin").and_then(|v| v.as_array()).and_then(|a| a.get(j)).and_then(util::num).unwrap_or(0.) };
        let mut rgba = vec![255u8; w * h * 4];
        for p in 0..w * h {
            for j in 0..4 {
                if j >= c {
                    continue;
                }
                let v = data[p * c + j];
                let t = (v - zmin(j)) / (zmax(j) - zmin(j));
                rgba[p * 4 + j] = (t.clamp(0., 1.) * 255.).round() as u8;
            }
            if c < 3 {
                rgba[p * 4 + 1] = rgba[p * 4];
                rgba[p * 4 + 2] = rgba[p * 4];
            }
        }
        let png = util::png_rgba(w, h, &rgba);
        (format!("data:image/png;base64,{}", util::base64_encode(&png)), w, h)
    };
    let x = (x0 - dx / 2., x0 + (w as f64 - 0.5) * dx);
    let y = (y0 - dy / 2., y0 + (h as f64 - 0.5) * dy);
    // plotly shows images top-down (the y axis reverses unless told
    // otherwise) with square pixels (y scale-anchored to x, domains shrink).
    let x_id = axes[t.xa].id.clone();
    let any_anchor = axes[t.xa].scaleanchor.is_some() || axes[t.ya].scaleanchor.is_some();
    let ya = &mut axes[t.ya];
    if ya.fixed_range.is_none() && !ya.autorange_given {
        ya.reversed = true;
    }
    if !any_anchor {
        ya.scaleanchor = Some(x_id);
        ya.scaleratio = 1.;
        ya.constrain_domain = true;
        axes[t.xa].constrain_domain = true;
    }
    Some(ImageCalc { href, x, y })
}

// ---------------------------------------------------------------- cross-trace

pub fn cross_trace(ctx: &Ctx, traces: &mut [Trace], axes: &mut [Axis]) {
    bin_histograms(ctx, traces, axes);
    position_bars(ctx, traces, axes);
    stack_scatter(traces);
    position_boxes(ctx, traces, axes);
    for i in 0..traces.len() {
        extremes(&traces[i], axes);
        if traces[i].kind != Kind::Pie && traces[i].xa < axes.len() {
            axes[traces[i].xa].has_data = true;
            axes[traces[i].ya].has_data = true;
        }
    }
}

struct BinSpec {
    start: f64,
    size: BinSize,
    end: f64,
}

#[derive(Clone, Copy)]
enum BinSize {
    Lin(f64),
    Months(i64),
}

fn bin_histograms(ctx: &Ctx, traces: &mut [Trace], axes: &[Axis]) {
    let barmode = ctx.lay_str("barmode").unwrap_or("group");
    // Group histograms sharing bins: same position axis + orientation, and
    // (unless overlay) all of them; with bingroup explicitly when given.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, t) in traces.iter().enumerate() {
        let Calc::Bars(b) = &t.calc else { continue };
        let Some(h) = &b.hist else { continue };
        let pa = if t.horizontal { t.ya } else { t.xa };
        let key = if !h.bingroup.is_empty() {
            // An explicit bingroup spans subplots (px facets).
            format!("{}-bg{}", t.horizontal, h.bingroup)
        } else if barmode == "overlay" {
            format!("trace{i}")
        } else {
            format!("{}-{}", pa, t.horizontal)
        };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    for (_, members) in groups {
        let first = members[0];
        let pa_i = if traces[first].horizontal { traces[first].ya } else { traces[first].xa };
        let pa = &axes[pa_i];
        let mut all: Vec<f64> = Vec::new();
        let mut nbins = None;
        let mut start = None;
        let mut end = None;
        let mut size = None;
        for &i in &members {
            if let Calc::Bars(b) = &traces[i].calc
                && let Some(h) = &b.hist
            {
                all.extend(h.data.iter().copied().filter(|v| v.is_finite()));
                nbins = nbins.or(h.nbins);
                start = start.or(h.start.clone());
                end = end.or(h.end.clone());
                size = size.or(h.size.clone());
            }
        }
        let spec = auto_bin(&all, pa, nbins, start.as_ref(), end.as_ref(), size.as_ref(), false);
        for &i in &members {
            let t = &mut traces[i];
            if let Calc::Bars(b) = &mut t.calc {
                apply_bins(b, &spec);
            }
        }
    }
}

fn auto_bin(data: &[f64], pa: &Axis, nbins: Option<f64>, start: Option<&Value>, end: Option<&Value>, size: Option<&Value>, is2d: bool) -> BinSpec {
    let Some((dmin, dmax)) = util::extent(data.iter().copied()) else {
        return BinSpec { start: 0., size: BinSize::Lin(1.), end: 1. };
    };
    if pa.ty == AxisType::Category {
        return BinSpec {
            start: dmin - 0.5,
            size: BinSize::Lin(size.and_then(util::num).map(|s| s.round().max(1.)).unwrap_or(1.)),
            end: dmax + 0.5,
        };
    }
    let is_date = pa.ty == AxisType::Date;
    let explicit_size = size.and_then(|s| match s {
        Value::String(st) if st.starts_with('M') => st[1..].parse::<i64>().ok().filter(|m| *m > 0).map(BinSize::Months),
        v => util::num_loose(v).filter(|x| *x > 0.).map(BinSize::Lin),
    });
    let (dtick, tick0) = match explicit_size {
        Some(s) => (s, if is_date { time::ms_from_ymd(2000, 1, 1) } else { 0. }),
        None => {
            let size0 = match nbins {
                Some(nb) => (dmax - dmin) / nb,
                None => {
                    let mut sorted: Vec<f64> = data.to_vec();
                    sorted.sort_by(|a, b| a.total_cmp(b));
                    sorted.dedup();
                    let min_diff = sorted.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min);
                    let min_diff = if min_diff.is_finite() && min_diff > 0. { min_diff } else { (dmax - dmin).max(1.) };
                    let msexp = 10f64.powf(min_diff.log10().floor());
                    // roundUp(minDiff/msexp, [0.9, 1.9, 4.9, 9.9], reverse): largest <= value
                    let r = min_diff / msexp;
                    let set = [0.9, 1.9, 4.9, 9.9];
                    let mut pick = set[0];
                    for s in set {
                        if s <= r {
                            pick = s;
                        }
                    }
                    let min_size = msexp * pick;
                    let n = data.len() as f64;
                    let mean = data.iter().sum::<f64>() / n;
                    let sd = (data.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
                    let s0 = min_size.max(2. * sd / n.powf(if is2d { 0.25 } else { 0.4 }));
                    if s0.is_finite() && s0 > 0. { s0 } else { 1. }
                }
            };
            let size0 = if size0 > 0. && size0.is_finite() { size0 } else { 1. };
            if is_date {
                let (d, t0) = ticks::plotly_date_dtick(size0);
                (
                    match d {
                        ticks::DateDtick::Ms(ms) => BinSize::Lin(ms),
                        ticks::DateDtick::Months(m) => BinSize::Months(m),
                    },
                    t0,
                )
            } else {
                (BinSize::Lin(ticks::plotly_linear_dtick(size0)), 0.)
            }
        }
    };
    let mut bin_start = match dtick {
        BinSize::Lin(d) => {
            // tickFirst on [dmin, dmax] then one step back.
            let first = ((dmin - tick0) / d).ceil() * d + tick0;
            first - d
        }
        BinSize::Months(m) => {
            let ticks = ticks::date_ticks(dmin - 400. * time::DAY * (m as f64), dmin, ticks::DateDtick::Months(m), tick0, 10_000);
            ticks.last().copied().unwrap_or(dmin)
        }
    };
    if let BinSize::Lin(d) = dtick {
        bin_start = auto_shift(bin_start, data, d, dmin, dmax, is_date);
    }
    let start_v = start.and_then(|v| if is_date { pa.r2l(v) } else { util::num_loose(v) });
    let end_v = end.and_then(|v| if is_date { pa.r2l(v) } else { util::num_loose(v) });
    let bin_start = start_v.unwrap_or(bin_start);
    let bin_end = match end_v {
        Some(e) => e,
        None => match dtick {
            BinSize::Lin(d) => {
                let count = 1. + ((dmax - bin_start) / d).floor();
                bin_start + count * d
            }
            BinSize::Months(m) => {
                let mut e = bin_start;
                let mut guard = 0;
                while e <= dmax && guard < 100_000 {
                    e = time::add_months(e, m);
                    guard += 1;
                }
                e
            }
        },
    };
    BinSpec {
        start: bin_start,
        size: dtick,
        end: bin_end,
    }
}

fn auto_shift(bin_start: f64, data: &[f64], dtick: f64, dmin: f64, dmax: f64, is_date: bool) -> f64 {
    let near_edge = |v: f64| (1. + (v - bin_start) * 100. / dtick).rem_euclid(100.) < 2.;
    let (mut edge, mut mid, mut ints) = (0usize, 0usize, 0usize);
    for &v in data {
        if v.fract() == 0. {
            ints += 1;
        }
        if near_edge(v) {
            edge += 1;
        }
        if near_edge(v + dtick / 2.) {
            mid += 1;
        }
    }
    let n = data.len();
    let mut start = bin_start;
    if ints == n && !is_date {
        if dtick < 1. {
            start = dmin - 0.5 * dtick;
        } else {
            start -= 0.5;
            if start + dtick < dmin {
                start += dtick;
            }
        }
    } else if (mid as f64) < edge as f64 * 0.1 && (edge as f64 > n as f64 * 0.3 || near_edge(dmin) || near_edge(dmax)) {
        let shift = dtick / 2.;
        start += if start + shift < dmin { shift } else { -shift };
    }
    start
}

/// Bin edges from start to end.
fn bin_edges(spec: &BinSpec) -> Vec<f64> {
    let mut edges = vec![spec.start];
    loop {
        let last = *edges.last().unwrap_or(&spec.start);
        if last >= spec.end - 1e-9 * (spec.end - spec.start).abs() || edges.len() > 100_000 {
            break;
        }
        let next = match spec.size {
            BinSize::Lin(d) => spec.start + d * edges.len() as f64,
            BinSize::Months(m) => time::add_months(last, m),
        };
        if next.is_nan() || next <= last {
            break;
        }
        edges.push(next);
    }
    if edges.len() < 2 {
        edges.push(spec.start + 1.);
    }
    edges
}

fn apply_bins(b: &mut BarCalc, spec: &BinSpec) {
    let Some(h) = b.hist.take() else { return };
    let edges = bin_edges(spec);
    let nb = edges.len() - 1;
    let mut counts = vec![0f64; nb];
    let mut nonempty = vec![0usize; nb];
    let mut mins = vec![f64::INFINITY; nb];
    let mut maxs = vec![f64::NEG_INFINITY; nb];
    let uniform = match spec.size {
        BinSize::Lin(d) => Some(d),
        _ => None,
    };
    let mut total = 0.;
    for (i, &v) in h.data.iter().enumerate() {
        if !v.is_finite() || v < edges[0] || v >= edges[nb] {
            continue;
        }
        let k = match uniform {
            Some(d) => (((v - spec.start) / d).floor().max(0.) as usize).min(nb - 1),
            None => edges.partition_point(|&e| e <= v).saturating_sub(1).min(nb - 1),
        };
        let w = h.weights.as_ref().map(|w| w.get(i).copied().unwrap_or(f64::NAN));
        match h.func.as_str() {
            "sum" | "avg" => {
                if let Some(w) = w.filter(|w| w.is_finite()) {
                    counts[k] += w;
                    nonempty[k] += 1;
                    total += w;
                } else if w.is_none() {
                    counts[k] += 1.;
                    nonempty[k] += 1;
                    total += 1.;
                }
            }
            "min" | "max" => {
                if let Some(w) = w.filter(|w| w.is_finite()) {
                    mins[k] = mins[k].min(w);
                    maxs[k] = maxs[k].max(w);
                    nonempty[k] += 1;
                }
            }
            _ => {
                counts[k] += 1.;
                nonempty[k] += 1;
                total += 1.;
            }
        }
    }
    match h.func.as_str() {
        "avg" => {
            for k in 0..nb {
                if nonempty[k] > 0 {
                    counts[k] /= nonempty[k] as f64;
                }
            }
        }
        "min" => counts = (0..nb).map(|k| if nonempty[k] > 0 { mins[k] } else { f64::NAN }).collect(),
        "max" => counts = (0..nb).map(|k| if nonempty[k] > 0 { maxs[k] } else { f64::NAN }).collect(),
        _ => {}
    }
    let widths: Vec<f64> = edges.windows(2).map(|w| w[1] - w[0]).collect();
    match h.norm.as_str() {
        "percent" => counts.iter_mut().for_each(|c| *c *= 100. / total.max(1e-300)),
        "probability" => counts.iter_mut().for_each(|c| *c /= total.max(1e-300)),
        "density" => counts.iter_mut().zip(&widths).for_each(|(c, w)| *c /= *w),
        "probability density" => counts.iter_mut().zip(&widths).for_each(|(c, w)| *c /= *w * total.max(1e-300)),
        _ => {}
    }
    if h.cumulative {
        let mut acc = 0.;
        for c in counts.iter_mut() {
            if c.is_finite() {
                acc += *c;
            }
            *c = acc;
        }
    }
    // plotly drops empty bins at both ends so autorange ignores them.
    let nonzero = |c: &f64| c.is_finite() && *c != 0.;
    let first = counts.iter().position(nonzero).unwrap_or(0);
    let last = counts.iter().rposition(nonzero).unwrap_or(nb.saturating_sub(1)).max(first);
    let counts = counts[first..=last].to_vec();
    let widths = widths[first..=last].to_vec();
    let edges = &edges[first..=last + 1];
    let nb = counts.len();
    b.pos = edges.windows(2).map(|w| (w[0] + w[1]) / 2.).collect();
    b.val = counts;
    b.base = vec![0.; nb];
    b.width = Some(Each::Many(widths));
    let _ = h.categorical;
}

fn position_bars(ctx: &Ctx, traces: &mut [Trace], axes: &mut [Axis]) {
    let barmode = ctx.lay_str("barmode").unwrap_or("group").to_string();
    // plotly: gapless when any histogram bins a non-category axis, unless
    // two bar traces share a subplot in group mode.
    let mut gapless = false;
    let mut gapped_anyway = false;
    let mut used: Vec<(usize, usize)> = Vec::new();
    for t in traces.iter().filter(|t| matches!(t.kind, Kind::Bar | Kind::Histogram) && !t.legend_only) {
        if barmode == "group" {
            if used.contains(&(t.xa, t.ya)) {
                gapped_anyway = true;
            }
            used.push((t.xa, t.ya));
        }
        let pa = if t.horizontal { t.ya } else { t.xa };
        if t.kind == Kind::Histogram && axes[pa].ty != AxisType::Category {
            gapless = true;
        }
    }
    let bargap = ctx.lay_num("bargap").unwrap_or(if gapless && !gapped_anyway { 0. } else { 0.2 }).clamp(0., 1.);
    let bargroupgap = ctx.lay_num("bargroupgap").unwrap_or(0.).clamp(0., 1.);
    let barnorm = ctx.lay_str("barnorm").unwrap_or("").to_string();
    // Group bar traces by (position axis, size axis, orientation).
    let mut groups: Vec<((usize, usize, bool), Vec<usize>)> = Vec::new();
    for (i, t) in traces.iter().enumerate() {
        if !matches!(t.calc, Calc::Bars(_)) || t.legend_only {
            continue;
        }
        let (pa, sa) = if t.horizontal { (t.ya, t.xa) } else { (t.xa, t.ya) };
        let key = (pa, sa, t.horizontal);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    for ((pa_i, _sa_i, _h), members) in groups {
        // Distinct positions -> minDiff.
        let mut all_pos: Vec<f64> = Vec::new();
        for &i in &members {
            if let Calc::Bars(b) = &traces[i].calc {
                all_pos.extend(b.pos.iter().copied().filter(|v| v.is_finite()));
            }
        }
        let n_all = all_pos.len();
        all_pos.sort_by(|a, b| a.total_cmp(b));
        all_pos.dedup_by(|a, b| (*a - *b).abs() < 1e-9 * (a.abs().max(1.)));
        let min_diff = all_pos.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min);
        let min_diff = if min_diff.is_finite() && min_diff > 0. { min_diff } else { 1. };
        let overlap = n_all != all_pos.len();
        let group_w = min_diff * (1. - bargap);
        let n = members.len() as f64;
        let mut offset_groups: Vec<String> = Vec::new();
        for &i in &members {
            if let Calc::Bars(b) = &traces[i].calc
                && !b.offsetgroup.is_empty()
                && !offset_groups.contains(&b.offsetgroup)
            {
                offset_groups.push(b.offsetgroup.clone());
            }
        }
        let use_offset_groups = barmode == "group" && !offset_groups.is_empty();
        let stack = barmode == "stack" || barmode == "relative";
        // Stacking accumulators per position (relative: separate +/-).
        let mut pos_acc: HashMap<u64, f64> = HashMap::new();
        let mut neg_acc: HashMap<u64, f64> = HashMap::new();
        let mut totals: HashMap<u64, f64> = HashMap::new();
        if !barnorm.is_empty() && stack {
            for &i in &members {
                if let Calc::Bars(b) = &traces[i].calc {
                    for (p, v) in b.pos.iter().zip(&b.val) {
                        if v.is_finite() {
                            *totals.entry(key(*p)).or_insert(0.) += v.abs();
                        }
                    }
                }
            }
        }
        for (k, &i) in members.iter().enumerate() {
            let Calc::Bars(b) = &mut traces[i].calc else { continue };
            let (w_each, center_off) = if barmode == "group" {
                let (slots, slot) = if use_offset_groups {
                    (offset_groups.len() as f64, offset_groups.iter().position(|g| *g == b.offsetgroup).unwrap_or(0) as f64)
                } else if overlap {
                    (n, k as f64)
                } else {
                    (1., 0.)
                };
                let w_gap = group_w / slots;
                let w = w_gap * (1. - bargroupgap);
                (w, ((2. * slot + 1. - slots) * w_gap - w) / 2. + w / 2.)
            } else {
                (group_w * (1. - bargroupgap).max(0.), 0.)
            };
            let len = b.pos.len();
            b.p0 = vec![f64::NAN; len];
            b.p1 = vec![f64::NAN; len];
            b.s0 = vec![f64::NAN; len];
            b.s1 = vec![f64::NAN; len];
            for j in 0..len {
                let p = b.pos[j];
                let v = b.val[j];
                if !p.is_finite() {
                    continue;
                }
                let (mut w, mut c) = (w_each, p + center_off);
                if let Some(ew) = &b.width {
                    let ww = ew.at(j);
                    if ww.is_finite() && ww > 0. {
                        // Explicit widths (histogram bins): scaled by the gaps.
                        w = if barmode == "group" && overlap && !use_offset_groups {
                            ww * (1. - bargap) / n * (1. - bargroupgap)
                        } else {
                            ww * (1. - bargap) * (1. - bargroupgap)
                        };
                        if barmode == "group" && overlap {
                            let slots = if use_offset_groups { offset_groups.len() as f64 } else { n };
                            let slot = if use_offset_groups {
                                offset_groups.iter().position(|g| *g == b.offsetgroup).unwrap_or(0) as f64
                            } else {
                                k as f64
                            };
                            let gw = ww * (1. - bargap) / slots;
                            c = p + ((2. * slot + 1. - slots) * gw) / 2.;
                        } else {
                            c = p;
                        }
                    }
                }
                if let Some(off) = &b.offset {
                    let o = off.at(j);
                    if o.is_finite() {
                        c = p + o + w / 2.;
                    }
                }
                b.p0[j] = c - w / 2.;
                b.p1[j] = c + w / 2.;
                if !v.is_finite() {
                    continue;
                }
                let mut v = v;
                if let Some(t) = totals.get(&key(p))
                    && *t > 0.
                {
                    v = v / t * if barnorm == "percent" { 100. } else { 1. };
                }
                if stack && !b.explicit_base {
                    let acc = if barmode == "relative" && v < 0. { &mut neg_acc } else { &mut pos_acc };
                    let base = *acc.get(&key(p)).unwrap_or(&0.);
                    b.s0[j] = base;
                    b.s1[j] = base + v;
                    acc.insert(key(p), base + v);
                } else {
                    let base = b.base.get(j).copied().unwrap_or(0.);
                    b.s0[j] = base;
                    b.s1[j] = base + v;
                }
            }
        }
        // Position-axis tick spacing floor and no default grid.
        let a = &mut axes[pa_i];
        a.no_grid_default = true;
        if a.ty != AxisType::Category && a.ty != AxisType::Date {
            let all_int = all_pos.iter().all(|p| p.fract() == 0.);
            if all_int || barmode != "overlay" {
                a.min_dtick = Some(min_diff);
            }
        }
        // Position extremes: every bar +- half the spacing.
        let vpad = min_diff / 2.;
        let mut pts = Vec::new();
        for &i in &members {
            if let Calc::Bars(b) = &traces[i].calc {
                for j in 0..b.pos.len() {
                    let p = b.pos[j];
                    if !p.is_finite() {
                        continue;
                    }
                    if b.width.is_some() || b.offset.is_some() {
                        pts.push(b.p0[j].min(p - vpad));
                        pts.push(b.p1[j].max(p + vpad));
                        if let Some(Each::Many(_)) = &b.width {
                            pts.push(b.p0[j]);
                            pts.push(b.p1[j]);
                        }
                    } else {
                        pts.push(p - vpad);
                        pts.push(p + vpad);
                    }
                }
            }
        }
        // Histograms span exactly their bins.
        let hist_only = members.iter().all(|&i| traces[i].kind == Kind::Histogram);
        if hist_only {
            pts.clear();
            for &i in &members {
                if let Calc::Bars(b) = &traces[i].calc
                    && let Some(Each::Many(w)) = &b.width
                {
                    for (p, w) in b.pos.iter().zip(w) {
                        pts.push(p - w / 2.);
                        pts.push(p + w / 2.);
                    }
                }
            }
        }
        axes[pa_i].expand(&pts, &PadSpec::Const(0.), ExtOpts { padded: false, tozero: false });
    }
}

fn key(p: f64) -> u64 {
    // Merge positions that differ only by float noise.
    ((p * 1e9).round()).to_bits()
}

fn stack_scatter(traces: &mut [Trace]) {
    // stackgroups: (xa, ya, group) -> members in order.
    let mut groups: Vec<((usize, usize, String), Vec<usize>)> = Vec::new();
    for (i, t) in traces.iter().enumerate() {
        if let Calc::Scatter(s) = &t.calc
            && let Some(g) = &s.stackgroup
            && !t.legend_only
        {
            let k = (t.xa, t.ya, g.clone());
            match groups.iter_mut().find(|(kk, _)| *kk == k) {
                Some((_, v)) => v.push(i),
                None => groups.push((k, vec![i])),
            }
        }
    }
    for (_, members) in groups {
        let mut acc: HashMap<u64, f64> = HashMap::new();
        let mut totals: HashMap<u64, f64> = HashMap::new();
        let norm = match &traces[members[0]].calc {
            Calc::Scatter(s) => s.groupnorm.clone(),
            _ => String::new(),
        };
        if !norm.is_empty() {
            for &i in &members {
                if let Calc::Scatter(s) = &traces[i].calc {
                    for (x, y) in s.x.iter().zip(&s.y) {
                        if y.is_finite() {
                            *totals.entry(key(*x)).or_insert(0.) += y;
                        }
                    }
                }
            }
        }
        let horizontal = traces[members[0]].horizontal;
        let mut prev: Option<(Vec<f64>, Vec<f64>)> = None;
        for (k, &i) in members.iter().enumerate() {
            let Calc::Scatter(s) = &mut traces[i].calc else { continue };
            let n = s.x.len();
            let mut base = vec![0.; n];
            #[allow(clippy::needless_range_loop)]
            for j in 0..n {
                let (pos, val) = if horizontal { (s.y[j], s.x[j]) } else { (s.x[j], s.y[j]) };
                if !pos.is_finite() {
                    continue;
                }
                let mut v = if val.is_finite() { val } else { 0. };
                if let Some(t) = totals.get(&key(pos))
                    && *t != 0.
                {
                    v = v / t * if norm == "percent" { 100. } else { 1. };
                }
                let b = *acc.get(&key(pos)).unwrap_or(&0.);
                base[j] = b;
                acc.insert(key(pos), b + v);
                if horizontal {
                    s.x[j] = b + v;
                } else {
                    s.y[j] = b + v;
                }
            }
            if k == 0 && matches!(s.fill, Fill::ToNextY | Fill::ToNextX) {
                s.fill = if horizontal { Fill::ToZeroX } else { Fill::ToZeroY };
            }
            s.base = Some(base);
            s.next_to = prev.take();
            prev = Some((s.x.clone(), s.y.clone()));
        }
    }
    // Plain (unstacked) tonexty / tonextx fills link to the previous trace.
    let mut prev_line: HashMap<(usize, usize), (Vec<f64>, Vec<f64>)> = HashMap::new();
    for t in traces.iter_mut() {
        let k = (t.xa, t.ya);
        if let Calc::Scatter(s) = &mut t.calc
            && s.stackgroup.is_none() {
                if matches!(s.fill, Fill::ToNextY | Fill::ToNextX | Fill::ToNext) {
                    match prev_line.get(&k) {
                        Some(p) => s.next_to = Some(p.clone()),
                        None => {
                            s.fill = match s.fill {
                                Fill::ToNextY => Fill::ToZeroY,
                                Fill::ToNextX => Fill::ToZeroX,
                                _ => Fill::None,
                            }
                        }
                    }
                }
                prev_line.insert(k, (s.x.clone(), s.y.clone()));
            }
    }
}

fn position_boxes(ctx: &Ctx, traces: &mut [Trace], axes: &mut [Axis]) {
    let boxmode = ctx.lay_str("boxmode").unwrap_or("overlay");
    let boxgap = ctx.lay_num("boxgap").unwrap_or(0.3);
    let boxgroupgap = ctx.lay_num("boxgroupgap").unwrap_or(0.3);
    let mut groups: Vec<((usize, bool), Vec<usize>)> = Vec::new();
    for (i, t) in traces.iter().enumerate() {
        if !matches!(t.calc, Calc::Box(_)) || t.legend_only {
            continue;
        }
        let pa = if t.horizontal { t.ya } else { t.xa };
        let k = (pa, t.horizontal);
        match groups.iter_mut().find(|(kk, _)| *kk == k) {
            Some((_, v)) => v.push(i),
            None => groups.push((k, vec![i])),
        }
    }
    for ((pa_i, _), members) in groups {
        let mut all: Vec<f64> = Vec::new();
        for &i in &members {
            if let Calc::Box(b) = &traces[i].calc {
                all.extend(b.boxes.iter().map(|x| x.pos));
            }
        }
        all.sort_by(|a, b| a.total_cmp(b));
        all.dedup();
        let dpos = all.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min);
        let dpos = if dpos.is_finite() && dpos > 0. { dpos } else { 1. };
        let mut offset_groups: Vec<String> = Vec::new();
        for &i in &members {
            if let Calc::Box(b) = &traces[i].calc
                && !offset_groups.contains(&b.offsetgroup)
            {
                offset_groups.push(b.offsetgroup.clone());
            }
        }
        let n = if boxmode == "group" { offset_groups.len().max(1) as f64 } else { 1. };
        let group_w = dpos * (1. - boxgap);
        let mut pts = Vec::new();
        for &i in &members {
            let Calc::Box(b) = &mut traces[i].calc else { continue };
            let slot = if boxmode == "group" { offset_groups.iter().position(|g| *g == b.offsetgroup).unwrap_or(0) as f64 } else { 0. };
            let w_gap = group_w / n;
            b.width = w_gap * (1. - if boxmode == "group" { boxgroupgap } else { 0. });
            b.offset = if boxmode == "group" { (2. * slot + 1. - n) * w_gap / 2. } else { 0. };
            if let Some(f) = b.width_frac {
                b.width = dpos * f;
                b.offset = 0.;
            }
            for bx in &b.boxes {
                let c = bx.pos + b.offset;
                pts.push(c - b.width / 2.);
                pts.push(c + b.width / 2.);
                if b.boxpoints == "all" {
                    let pp = c + b.pointpos * b.width / 2.;
                    pts.push(pp - b.jitter * b.width / 2.);
                    pts.push(pp + b.jitter * b.width / 2.);
                }
            }
        }
        let a = &mut axes[pa_i];
        a.no_grid_default = true;
        // plotly pads box positions by dPos/2 around the outermost boxes.
        let lo = all.first().copied().unwrap_or(0.) - dpos / 2.;
        let hi = all.last().copied().unwrap_or(0.) + dpos / 2.;
        pts.push(lo);
        pts.push(hi);
        a.expand(&pts, &PadSpec::Const(0.), ExtOpts { padded: false, tozero: false });
    }
}

fn extremes(t: &Trace, axes: &mut [Axis]) {
    if t.kind == Kind::Pie || t.legend_only {
        return;
    }
    match &t.calc {
        Calc::Scatter(s) => {
            let has_markers_or_text = s.markers || s.text;
            let n = s.x.len();
            let open_ended = n < 2 || s.x[0] != s.x[n - 1] || s.y[0] != s.y[n - 1];
            let mut xo = ExtOpts { padded: true, tozero: false };
            let mut yo = ExtOpts { padded: true, tozero: false };
            let first_scatter = s.next_to.is_none();
            if open_ended && (s.fill == Fill::ToZeroX || (s.fill == Fill::ToNextX && first_scatter)) {
                xo.tozero = true;
            } else if s.err_y.is_none() && (matches!(s.fill, Fill::ToNextY | Fill::ToZeroY) || !has_markers_or_text) {
                xo.padded = false;
            }
            if open_ended && (s.fill == Fill::ToZeroY || (s.fill == Fill::ToNextY && first_scatter)) {
                yo.tozero = true;
            } else if matches!(s.fill, Fill::ToNextX | Fill::ToZeroX) {
                yo.padded = false;
            }
            let ppad = if s.markers { s.marker.ppad.clone() } else { PadSpec::Const(0.) };
            let xpad = if xo.padded { ppad.clone() } else { PadSpec::Const(0.) };
            let mut xs = s.x.clone();
            let mut ys = s.y.clone();
            if let Some(e) = &s.err_x {
                let xa = &axes[t.xa];
                for i in 0..n {
                    if xa.ty == AxisType::Linear {
                        if e.plus[i].is_finite() {
                            xs.push(s.x[i] + e.plus[i]);
                        }
                        if e.minus[i].is_finite() {
                            xs.push(s.x[i] - e.minus[i]);
                        }
                    }
                }
            }
            if let Some(e) = &s.err_y {
                let ya = &axes[t.ya];
                for i in 0..n {
                    if ya.ty == AxisType::Linear {
                        if e.plus.get(i).is_some_and(|v| v.is_finite()) {
                            ys.push(s.y[i] + e.plus[i]);
                        }
                        if e.minus.get(i).is_some_and(|v| v.is_finite()) {
                            ys.push(s.y[i] - e.minus[i]);
                        }
                    }
                }
            }
            if let Some(base) = &s.base {
                if t.horizontal {
                    xs.extend(base.iter().copied());
                } else {
                    ys.extend(base.iter().copied());
                }
            }
            let pad_for = |p: &PadSpec, len: usize| match p {
                PadSpec::Each(v) if v.len() != len => {
                    let mut v = v.clone();
                    v.resize(len, 0.);
                    PadSpec::Each(v)
                }
                other => other.clone(),
            };
            let xl = xs.len();
            let yl = ys.len();
            axes[t.xa].expand(&xs, &pad_for(&xpad, xl), xo);
            axes[t.ya].expand(&ys, &pad_for(&ppad, yl), yo);
        }
        Calc::Bars(b) => {
            let sa = if t.horizontal { t.xa } else { t.ya };
            let mut vals = Vec::with_capacity(b.s0.len() * 2);
            for (a, c) in b.s0.iter().zip(&b.s1) {
                if a.is_finite() && c.is_finite() {
                    vals.push(*a);
                    vals.push(*c);
                }
            }
            let is_log = axes[sa].ty == AxisType::Log;
            if is_log {
                vals = vals.into_iter().filter(|v| *v > 0.).map(f64::log10).collect();
            }
            axes[sa].expand(&vals, &PadSpec::Const(0.), ExtOpts { padded: true, tozero: !b.explicit_base });
        }
        Calc::Box(b) => {
            let va = if t.horizontal { t.xa } else { t.ya };
            let mut vals = Vec::new();
            for bx in &b.boxes {
                vals.push(bx.lf);
                vals.push(bx.uf);
                if let (Some(lo), Some(hi)) = (bx.points.first(), bx.points.last())
                    && b.boxpoints != "false"
                {
                    vals.push(*lo);
                    vals.push(*hi);
                }
                if b.boxmean == "sd" {
                    vals.push(bx.mean - bx.sd);
                    vals.push(bx.mean + bx.sd);
                }
            }
            axes[va].expand(&vals, &PadSpec::Const(0.), ExtOpts { padded: true, tozero: false });
        }
        Calc::Heatmap(h) => {
            axes[t.xa].expand(&[h.xe[0], h.xe[h.xe.len() - 1]], &PadSpec::Const(0.), ExtOpts::default());
            axes[t.ya].expand(&[h.ye[0], h.ye[h.ye.len() - 1]], &PadSpec::Const(0.), ExtOpts::default());
        }
        Calc::Image(im) => {
            axes[t.xa].expand(&[im.x.0, im.x.1], &PadSpec::Const(0.), ExtOpts::default());
            axes[t.ya].expand(&[im.y.0, im.y.1], &PadSpec::Const(0.), ExtOpts::default());
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- colorbars

pub struct Colorbar {
    pub scale: Colorscale,
    pub cmin: f64,
    pub cmax: f64,
    pub title: String,
    pub thickness: f64,
    pub label_w: f64,
    pub labels: Vec<(f64, String)>,
}

impl Colorbar {
    pub fn width(&self) -> f64 {
        // xpad + thickness + gap + labels + right pad
        10. + self.thickness + 4. + self.label_w + 10.
    }

    pub fn draw(&self, ctx: &Ctx, svg: &mut Svg, x: f64, area: &super::Area) {
        let th = &ctx.theme;
        let fs = th.font_size;
        let has_title = !self.title.is_empty();
        let top = area.t + 10. + if has_title { fs * 1.6 } else { 0. };
        let bottom = area.b() - 10.;
        if bottom - top < 10. {
            return;
        }
        let bx = x + 10.;
        // Gradient as thin strips (exact colors, renderer-independent).
        let strips = 64;
        let h = (bottom - top) / strips as f64;
        for k in 0..strips {
            let t = 1. - (k as f64 + 0.5) / strips as f64;
            svg.fill_rect(bx, top + k as f64 * h, self.thickness, h + 0.6, self.scale.at(t));
        }
        let style = TextStyle::new(fs, th.font_color);
        for (v, label) in &self.labels {
            let t = (v - self.cmin) / (self.cmax - self.cmin);
            let y = bottom - t * (bottom - top);
            svg.text(bx + self.thickness + 4., y + fs * 0.35, label, &style);
        }
        if has_title {
            let lines = text::rich(&self.title);
            let mut y = area.t + fs;
            for l in &lines {
                svg.text_runs(bx, y, l, &style);
                y += fs * 1.3;
            }
        }
    }
}

pub fn colorbars(ctx: &Ctx, traces: &[Trace]) -> Vec<Colorbar> {
    let mut out: Vec<Colorbar> = Vec::new();
    let mut seen_axes: Vec<String> = Vec::new();
    for t in traces {
        if t.legend_only {
            continue;
        }
        let cm = match (&t.calc, t.kind) {
            (Calc::Heatmap(h), _) => {
                let flat: Vec<f64> = h.z.iter().flatten().filter_map(|v| *v).collect();
                Some(color_map(ctx, t, "", &flat, true))
            }
            (Calc::Scatter(_), _) | (Calc::Bars(_), _) => {
                let arr = t.arr("marker.color");
                match arr {
                    Some(a) if !a.is_empty() && (0..a.len().min(20)).all(|i| matches!(a.get(i), Datum::Num(_) | Datum::Null)) => {
                        let vals: Vec<f64> = (0..a.len()).map(|i| a.num(i).unwrap_or(f64::NAN)).collect();
                        Some(color_map(ctx, t, "marker.", &vals, false))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(cm) = cm else { continue };
        if !cm.showscale {
            continue;
        }
        if let Some(ax) = &cm.axis {
            if seen_axes.contains(ax) {
                continue;
            }
            seen_axes.push(ax.clone());
        }
        // Ticks: plotly treats the bar as a y axis of ~plot height.
        let len = 220.;
        let nt = ticks::plotly_nticks(len, true, None);
        let dtick = ticks::plotly_linear_dtick((cm.cmax - cm.cmin).abs() / nt);
        let ef = ExpFormat::default();
        let (tr, ex) = format::plotly_tick_round(dtick, (cm.cmin, cm.cmax), &ef);
        let labels: Vec<(f64, String)> = ticks::linear_ticks(cm.cmin, cm.cmax, dtick, 0., 200)
            .into_iter()
            .map(|v| (v, format::plotly_num(v, tr, ex, &ef)))
            .collect();
        let fs = ctx.theme.font_size;
        let label_w = labels.iter().map(|(_, l)| text::width(l, fs, false)).fold(0., f64::max);
        let title_w = text::width(&text::plain(&cm.title), fs, false) - 30. - 4.;
        out.push(Colorbar {
            scale: cm.scale,
            cmin: cm.cmin,
            cmax: cm.cmax,
            title: cm.title,
            thickness: 30.,
            label_w: label_w.max(title_w),
            labels,
        });
    }
    out
}

// ---------------------------------------------------------------- drawing

pub fn draw(ctx: &Ctx, t: &Trace, xa: &Axis, ya: &Axis, svg: &mut Svg, notes: &mut Vec<String>) {
    let grouped = t.opacity < 0.999;
    if grouped {
        svg.group_opacity(t.opacity);
    }
    match &t.calc {
        Calc::Scatter(s) => draw_scatter(ctx, t, s, xa, ya, svg, notes),
        Calc::Bars(b) => draw_bars(ctx, t, b, xa, ya, svg),
        Calc::Box(b) => draw_box(ctx, t, b, xa, ya, svg),
        Calc::Heatmap(h) => draw_heatmap(h, xa, ya, svg),
        Calc::Image(im) => draw_image(im, xa, ya, svg),
        _ => {}
    }
    if grouped {
        svg.group_close();
    }
}

/// Visible-range pixel coordinates; None for gaps.
fn project(xa: &Axis, ya: &Axis, x: &[f64], y: &[f64]) -> Vec<Option<(f64, f64)>> {
    x.iter()
        .zip(y)
        .map(|(&a, &b)| {
            if a.is_finite() && b.is_finite() {
                let p = (xa.l2p(a), ya.l2p(b));
                // Keep wildly out-of-range points finite for the SVG.
                Some((p.0.clamp(-1e6, 1e6), p.1.clamp(-1e6, 1e6)))
            } else {
                None
            }
        })
        .collect()
}

/// Splits into connected runs (gaps break lines unless connectgaps).
fn segments(pts: &[Option<(f64, f64)>], connect: bool) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut cur: Vec<(f64, f64)> = Vec::new();
    for p in pts {
        match p {
            Some(p) => cur.push(*p),
            None => {
                if !connect && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Min/max-per-pixel-column decimation for long monotone-x lines.
fn decimate(seg: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if seg.len() < 4000 {
        return seg.to_vec();
    }
    let monotone = seg.windows(2).all(|w| w[1].0 >= w[0].0) || seg.windows(2).all(|w| w[1].0 <= w[0].0);
    if !monotone {
        // Drop points closer than 0.5px to the previously kept one.
        let mut out = vec![seg[0]];
        for &p in &seg[1..] {
            let l = out[out.len() - 1];
            if (p.0 - l.0).abs() >= 0.5 || (p.1 - l.1).abs() >= 0.5 {
                out.push(p);
            }
        }
        if out.last() != seg.last() {
            out.push(seg[seg.len() - 1]);
        }
        return out;
    }
    let mut out = Vec::with_capacity(4096);
    let mut i = 0;
    while i < seg.len() {
        let col = seg[i].0.floor();
        let first = seg[i];
        let (mut lo, mut hi) = (first, first);
        let mut last = first;
        let mut j = i;
        while j < seg.len() && seg[j].0.floor() == col {
            let p = seg[j];
            if p.1 < lo.1 {
                lo = p;
            }
            if p.1 > hi.1 {
                hi = p;
            }
            last = p;
            j += 1;
        }
        out.push(first);
        let (a, b) = if lo.0 <= hi.0 { (lo, hi) } else { (hi, lo) };
        if a != first {
            out.push(a);
        }
        if b != a && b != first {
            out.push(b);
        }
        if last != b && last != first {
            out.push(last);
        }
        i = j;
    }
    out
}

fn line_path(d: &mut PathData, seg: &[(f64, f64)], shape: LineShape, reverse: bool, continue_path: bool) {
    let pts: Vec<(f64, f64)> = if reverse { seg.iter().rev().copied().collect() } else { seg.to_vec() };
    if pts.is_empty() {
        return;
    }
    if continue_path {
        d.line_to(pts[0].0, pts[0].1);
    } else {
        d.move_to(pts[0].0, pts[0].1);
    }
    match shape {
        LineShape::Linear => {
            for p in &pts[1..] {
                d.line_to(p.0, p.1);
            }
        }
        LineShape::Hv | LineShape::Vh | LineShape::Hvh | LineShape::Vhv => {
            for w in pts.windows(2) {
                let (a, b) = (w[0], w[1]);
                match shape {
                    LineShape::Hv => {
                        d.line_to(b.0, a.1);
                        d.line_to(b.0, b.1);
                    }
                    LineShape::Vh => {
                        d.line_to(a.0, b.1);
                        d.line_to(b.0, b.1);
                    }
                    LineShape::Hvh => {
                        let m = (a.0 + b.0) / 2.;
                        d.line_to(m, a.1);
                        d.line_to(m, b.1);
                        d.line_to(b.0, b.1);
                    }
                    _ => {
                        let m = (a.1 + b.1) / 2.;
                        d.line_to(a.0, m);
                        d.line_to(b.0, m);
                        d.line_to(b.0, b.1);
                    }
                }
            }
        }
        LineShape::Spline => {
            if pts.len() < 3 {
                for p in &pts[1..] {
                    d.line_to(p.0, p.1);
                }
                return;
            }
            // plotly's smoothing (Catmull-Rom style with smoothing 1).
            let s = 1.0 * 0.5;
            let n = pts.len();
            let ctrl = |i: usize| -> ((f64, f64), (f64, f64)) {
                let p0 = pts[i.saturating_sub(1)];
                let p1 = pts[i];
                let p2 = pts[(i + 1).min(n - 1)];
                let d01 = ((p1.0 - p0.0).powi(2) + (p1.1 - p0.1).powi(2)).sqrt();
                let d12 = ((p2.0 - p1.0).powi(2) + (p2.1 - p1.1).powi(2)).sqrt();
                let tot = (d01 + d12).max(1e-9);
                let (dx, dy) = (p2.0 - p0.0, p2.1 - p0.1);
                let f0 = s * d01 / tot;
                let f1 = s * d12 / tot;
                ((p1.0 - dx * f0 * 0.5, p1.1 - dy * f0 * 0.5), (p1.0 + dx * f1 * 0.5, p1.1 + dy * f1 * 0.5))
            };
            for i in 0..n - 1 {
                let c1 = if i == 0 { pts[0] } else { ctrl(i).1 };
                let c2 = if i + 1 == n - 1 { pts[n - 1] } else { ctrl(i + 1).0 };
                d.cubic_to(c1, c2, pts[i + 1]);
            }
        }
    }
}

fn draw_scatter(ctx: &Ctx, t: &Trace, s: &ScatterCalc, xa: &Axis, ya: &Axis, svg: &mut Svg, notes: &mut Vec<String>) {
    let pts = project(xa, ya, &s.x, &s.y);
    let segs = segments(&pts, s.connectgaps);
    let segs: Vec<Vec<(f64, f64)>> = segs.iter().map(|sg| decimate(sg)).collect();
    // Fill.
    if s.fill != Fill::None && s.fillcolor.a > 0. {
        let mut d = PathData::new();
        match s.fill {
            Fill::ToZeroY | Fill::ToZeroX => {
                let zero_y = ya.l2p(if ya.ty == AxisType::Log { ya.range.0.min(ya.range.1) } else { 0. });
                let zero_x = xa.l2p(if xa.ty == AxisType::Log { xa.range.0.min(xa.range.1) } else { 0. });
                for sg in &segs {
                    if sg.is_empty() {
                        continue;
                    }
                    if s.fill == Fill::ToZeroY {
                        d.move_to(sg[0].0, zero_y);
                        line_path(&mut d, sg, s.shape, false, true);
                        d.line_to(sg[sg.len() - 1].0, zero_y);
                    } else {
                        d.move_to(zero_x, sg[0].1);
                        line_path(&mut d, sg, s.shape, false, true);
                        d.line_to(zero_x, sg[sg.len() - 1].1);
                    }
                    d.close();
                }
            }
            Fill::ToSelf => {
                for sg in &segs {
                    line_path(&mut d, sg, s.shape, false, false);
                    d.close();
                }
            }
            Fill::ToNextY | Fill::ToNextX | Fill::ToNext => {
                let prev = match (&s.base, &s.next_to) {
                    (Some(base), _) if s.stackgroup.is_some() => {
                        let (bx, by) = if t.horizontal { (base.clone(), s.y.clone()) } else { (s.x.clone(), base.clone()) };
                        Some(project(xa, ya, &bx, &by))
                    }
                    (_, Some((px, py))) => Some(project(xa, ya, px, py)),
                    _ => None,
                };
                if let Some(prev) = prev {
                    let cur: Vec<(f64, f64)> = pts.iter().flatten().copied().collect();
                    let prv: Vec<(f64, f64)> = prev.iter().flatten().copied().collect();
                    if !cur.is_empty() && !prv.is_empty() {
                        line_path(&mut d, &cur, s.shape, false, false);
                        line_path(&mut d, &prv, s.shape, true, true);
                        d.close();
                    }
                }
            }
            Fill::None => {}
        }
        svg.path(&d.d, &Style::fill(s.fillcolor));
    }
    // Error bars (under markers).
    for (e, is_y) in [(&s.err_y, true), (&s.err_x, false)] {
        let Some(e) = e else { continue };
        let mut d = PathData::new();
        for (i, p) in pts.iter().enumerate() {
            let Some(p) = p else { continue };
            let (plus, minus) = (e.plus.get(i).copied().unwrap_or(f64::NAN), e.minus.get(i).copied().unwrap_or(f64::NAN));
            if is_y {
                let v = s.y[i];
                let (a, b) = (ya.l2p(v + if plus.is_finite() { plus } else { 0. }), ya.l2p(v - if minus.is_finite() { minus } else { 0. }));
                d.move_to(p.0, a);
                d.line_to(p.0, b);
                if plus.is_finite() {
                    d.move_to(p.0 - e.width, a);
                    d.line_to(p.0 + e.width, a);
                }
                if minus.is_finite() {
                    d.move_to(p.0 - e.width, b);
                    d.line_to(p.0 + e.width, b);
                }
            } else {
                let v = s.x[i];
                let (a, b) = (xa.l2p(v + if plus.is_finite() { plus } else { 0. }), xa.l2p(v - if minus.is_finite() { minus } else { 0. }));
                d.move_to(a, p.1);
                d.line_to(b, p.1);
                if plus.is_finite() {
                    d.move_to(a, p.1 - e.width);
                    d.line_to(a, p.1 + e.width);
                }
                if minus.is_finite() {
                    d.move_to(b, p.1 - e.width);
                    d.line_to(b, p.1 + e.width);
                }
            }
        }
        svg.path(&d.d, &Style::stroke(e.color, e.thickness));
    }
    // Lines.
    if s.lines && s.line_width > 0. {
        let mut d = PathData::new();
        for sg in &segs {
            if sg.len() >= 2 {
                line_path(&mut d, sg, s.shape, false, false);
            }
        }
        let mut style = Style::stroke(s.line_color, s.line_width);
        style.dash = s.dash.clone();
        style.round_joins = true;
        svg.path(&d.d, &style);
    }
    if s.markers {
        draw_markers(ctx, &s.marker, &pts, svg, notes);
    }
    if s.text && !s.texts.is_empty() {
        let style = TextStyle::new(s.text_size, s.text_color);
        for (i, p) in pts.iter().enumerate() {
            let (Some(p), Some(txt)) = (p, s.texts.get(i)) else { continue };
            if txt.is_empty() {
                continue;
            }
            let r = s.marker.radius.at(i);
            let (v, h) = s.textpos.split_once(' ').unwrap_or(("middle", "center"));
            let (dx, anchor) = match h {
                "left" => (-(r + 2.), Anchor::End),
                "right" => (r + 2., Anchor::Start),
                _ => (0., Anchor::Middle),
            };
            let dy = match v {
                "top" => -(r + 3.),
                "bottom" => r + 3. + s.text_size * 0.75,
                _ => s.text_size * 0.35,
            };
            svg.text_runs(p.0 + dx, p.1 + dy, &text::rich(txt).into_iter().flatten().collect::<Vec<_>>(), &style.clone().anchor(anchor));
        }
    }
}

pub fn draw_markers(ctx: &Ctx, m: &Marker, pts: &[Option<(f64, f64)>], svg: &mut Svg, notes: &mut Vec<String>) {
    let count = pts.iter().filter(|p| p.is_some()).count();
    let outline = |c: Rgba| ctx.theme.outline(c);
    if count <= COMPACT_MARKERS {
        for (i, p) in pts.iter().enumerate() {
            let Some((x, y)) = p else { continue };
            let r = m.radius.at(i);
            if r.is_nan() || r <= 0. {
                continue;
            }
            let sym = m.symbol.at(i);
            let color = m.color.at(i);
            let op = m.opacity.at(i);
            let lc = if m.line_width > 0. { outline(m.line_color.at(i)) } else { None };
            let mut style = if sym.open || sym.is_line() {
                Style::stroke(color, m.line_width.max(1.))
            } else {
                let mut st = Style::fill(color);
                if let Some(lc) = lc {
                    st.stroke = Some(lc);
                    st.stroke_width = m.line_width;
                }
                st
            };
            style.opacity = if op.is_finite() { op } else { 1. };
            if sym.shape == Shape::Circle && !sym.open {
                svg.circle(*x, *y, r, &style);
            } else {
                let mut d = PathData::new();
                sym.path(&mut d, *x, *y, r);
                svg.path(&d.d, &style);
            }
        }
        return;
    }
    // Compact output for big scatters: one path per (color, size, symbol),
    // points de-duplicated on a 0.5 px grid.
    // (rgba, radius bits, symbol, opacity bits)
    type Key = (u32, u64, Symbol, u64);
    let mut groups: Vec<(Key, Vec<(f64, f64)>)> = Vec::new();
    let mut index: HashMap<Key, usize> = HashMap::new();
    let mut seen: std::collections::HashSet<(i64, i64, usize)> = std::collections::HashSet::new();
    let mut drawn = 0usize;
    for (i, p) in pts.iter().enumerate() {
        let Some((x, y)) = p else { continue };
        let r = m.radius.at(i);
        if r.is_nan() || r <= 0. {
            continue;
        }
        let c = m.color.at(i);
        let ckey = ((c.r as u32) << 24) | ((c.g as u32) << 16) | ((c.b as u32) << 8) | (c.a * 255.) as u32;
        let op = m.opacity.at(i);
        let k = (ckey, (r * 4.).round().to_bits(), m.symbol.at(i), (op * 100.).round().to_bits());
        let gi = *index.entry(k).or_insert_with(|| {
            groups.push((k, Vec::new()));
            groups.len() - 1
        });
        // Identical positions only look different when markers are translucent.
        let opaque = op >= 0.99 && c.a >= 0.99;
        if opaque && !seen.insert(((x * 2.).round() as i64, (y * 2.).round() as i64, gi)) {
            continue;
        }
        drawn += 1;
        if drawn > MAX_MARKERS {
            continue;
        }
        groups[gi].1.push((*x, *y));
    }
    if drawn > MAX_MARKERS {
        push_note(notes, &format!("showing {MAX_MARKERS} of {drawn} distinct marker positions"));
    }
    for ((ckey, rbits, sym, opbits), list) in groups {
        let color = Rgba {
            r: (ckey >> 24) as u8,
            g: (ckey >> 16) as u8,
            b: (ckey >> 8) as u8,
            a: (ckey & 0xff) as f32 / 255.,
        };
        let r = f64::from_bits(rbits) / 4.;
        let op = f64::from_bits(opbits) / 100.;
        // One path paints its overlaps once; translucent markers are split
        // into interleaved paths so dense areas still darken like plotly's.
        let chunks = if op < 0.99 || color.a < 0.99 { 8.min(list.len().max(1)) } else { 1 };
        for c in 0..chunks {
            let mut d = PathData::with_decimals(1);
            let pts = list.iter().skip(c).step_by(chunks);
            if sym.shape == Shape::Circle && !sym.open {
                // Zero-length round-capped strokes draw filled dots compactly.
                for (x, y) in pts {
                    d.move_to(*x, *y);
                    d.d.push_str("h0");
                }
                let mut style = Style::stroke(color, r * 2.);
                style.round_joins = true;
                style.opacity = op;
                svg.path(&d.d, &style);
            } else {
                for (x, y) in pts {
                    sym.path(&mut d, *x, *y, r);
                }
                let mut style = if sym.open { Style::stroke(color, m.line_width.max(1.)) } else { Style::fill(color) };
                style.opacity = op;
                svg.path(&d.d, &style);
            }
        }
    }
}

fn draw_bars(ctx: &Ctx, t: &Trace, b: &BarCalc, xa: &Axis, ya: &Axis, svg: &mut Svg) {
    let (pa, sa) = if t.horizontal { (ya, xa) } else { (xa, ya) };
    let n = b.p0.len();
    let lw = b.line_width;
    let mut text_jobs = Vec::new();
    let sconv = |v: f64| if sa.ty == AxisType::Log { if v > 0. { v.log10() } else { sa.range.0.min(sa.range.1) - 10. } } else { v };
    // Batch identical-style bars into one path.
    let mut batches: Vec<((Rgba, Option<Rgba>), PathData)> = Vec::new();
    for i in 0..n {
        let (p0, p1, s0, s1) = (b.p0[i], b.p1[i], b.s0[i], b.s1[i]);
        if !(p0.is_finite() && p1.is_finite() && s0.is_finite() && s1.is_finite()) {
            continue;
        }
        let (pp0, pp1) = (pa.l2p(p0), pa.l2p(p1));
        let (ss0, ss1) = (sa.l2p(sconv(s0)).clamp(-1e5, 1e5), sa.l2p(sconv(s1)).clamp(-1e5, 1e5));
        let (x0, x1, y0, y1) = if t.horizontal { (ss0, ss1, pp0, pp1) } else { (pp0, pp1, ss0, ss1) };
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));
        if s0 == s1 {
            continue;
        }
        let fill = b.color.at(i);
        let line = if lw > 0. { ctx.theme.outline(b.line_color.at(i)) } else { None };
        let k = (fill, line);
        let d = match batches.iter_mut().find(|(kk, _)| *kk == k) {
            Some((_, d)) => d,
            None => {
                batches.push((k, PathData::new()));
                &mut batches.last_mut().expect("just pushed").1
            }
        };
        d.rect(x0, y0, x1 - x0, y1 - y0);
        if let Some(txt) = b.texts.get(i).filter(|s| !s.is_empty()) {
            text_jobs.push((x0, x1, y0, y1, txt.clone(), fill, s1 < s0));
        }
    }
    for ((fill, line), d) in batches {
        let mut style = Style::fill(fill);
        if let Some(l) = line {
            style.stroke = Some(l);
            style.stroke_width = lw;
        }
        style.opacity = b.opacity;
        svg.path(&d.d, &style);
    }
    if b.textposition == "none" {
        return;
    }
    let fs = b.textfont_size;
    for (x0, x1, y0, y1, txt, fill, negative) in text_jobs {
        let w = text::width(&txt, fs, false);
        let (bw, bh) = (x1 - x0, y1 - y0);
        let fits = if t.horizontal { w + 6. < bw && fs * 1.2 < bh } else { w + 4. < bw && fs * 1.2 + 4. < bh };
        let pos = b.textposition.as_str();
        let inside = pos == "inside" || (pos == "auto" && fits);
        let outside = pos == "outside" || (pos == "auto" && !fits);
        let color = b.textfont_color.unwrap_or(if inside {
            fill.contrast_text()
        } else {
            ctx.theme.font_color
        });
        let style = TextStyle::new(fs, color).anchor(Anchor::Middle);
        if t.horizontal {
            let cy = (y0 + y1) / 2. + fs * 0.35;
            if inside {
                let x = if negative { x0 + 3. + w / 2. } else { x1 - 3. - w / 2. };
                svg.text(x, cy, &txt, &style);
            } else if outside {
                let x = if negative { x0 - 3. - w / 2. } else { x1 + 3. + w / 2. };
                svg.text(x, cy, &txt, &style);
            }
        } else {
            let cx = (x0 + x1) / 2.;
            if inside {
                let y = if negative { y1 - 4. } else { y0 + 4. + fs * 0.8 };
                if pos == "inside" && !fits {
                    // plotly shrinks text to fit; keep it readable instead.
                }
                svg.text(cx, y, &txt, &style);
            } else if outside {
                let y = if negative { y1 + 2. + fs * 0.8 } else { y0 - 3. };
                svg.text(cx, y, &txt, &style);
            }
        }
    }
}

fn draw_box(ctx: &Ctx, t: &Trace, b: &BoxCalc, xa: &Axis, ya: &Axis, svg: &mut Svg) {
    let (pa, va) = if t.horizontal { (ya, xa) } else { (xa, ya) };
    let line = Style {
        fill: Some(b.fillcolor),
        stroke: Some(b.line_color),
        stroke_width: b.line_width,
        opacity: 1.,
        ..Default::default()
    };
    let pt = |p: f64, v: f64| -> (f64, f64) {
        if t.horizontal { (va.l2p(v), pa.l2p(p)) } else { (pa.l2p(p), va.l2p(v)) }
    };
    let mut rng = Lcg(t.idx as u64 * 7919 + 17);
    for (k, bx) in b.boxes.iter().enumerate() {
        let c = bx.pos + b.offset;
        let hw = b.width / 2.;
        let mut line = line.clone();
        if let Some((lc, fc)) = b.colors.as_ref().and_then(|cs| cs.get(k)) {
            line.stroke = Some(*lc);
            line.fill = Some(*fc);
        }
        if b.ohlc {
            // low-high bar, open tick to the left, close tick to the right
            let mut d = PathData::new();
            let (a, z) = (pt(c, bx.lf), pt(c, bx.uf));
            d.move_to(a.0, a.1);
            d.line_to(z.0, z.1);
            let (o0, o1) = (pt(c - hw, bx.mean), pt(c, bx.mean));
            d.move_to(o0.0, o0.1);
            d.line_to(o1.0, o1.1);
            let (c0, c1) = (pt(c, bx.med), pt(c + hw, bx.med));
            d.move_to(c0.0, c0.1);
            d.line_to(c1.0, c1.1);
            let mut st = line.clone();
            st.fill = None;
            svg.path(&d.d, &st);
            continue;
        }
        let mut d = PathData::new();
        let corners = [pt(c - hw, bx.q1), pt(c + hw, bx.q1), pt(c + hw, bx.q3), pt(c - hw, bx.q3)];
        d.move_to(corners[0].0, corners[0].1);
        for p in &corners[1..] {
            d.line_to(p.0, p.1);
        }
        d.close();
        // median
        let (m0, m1) = (pt(c - hw, bx.med), pt(c + hw, bx.med));
        d.move_to(m0.0, m0.1);
        d.line_to(m1.0, m1.1);
        // whiskers + caps
        let ww = hw * b.whiskerwidth;
        for (from, to) in [(bx.q1, bx.lf), (bx.q3, bx.uf)] {
            let (a, z) = (pt(c, from), pt(c, to));
            d.move_to(a.0, a.1);
            d.line_to(z.0, z.1);
            if ww > 0. {
                let (k0, k1) = (pt(c - ww, to), pt(c + ww, to));
                d.move_to(k0.0, k0.1);
                d.line_to(k1.0, k1.1);
            }
        }
        svg.path(&d.d, &line);
        if !b.boxmean.is_empty() {
            let mut d = PathData::new();
            let (a, z) = (pt(c - hw, bx.mean), pt(c + hw, bx.mean));
            d.move_to(a.0, a.1);
            d.line_to(z.0, z.1);
            let mut st = Style::stroke(b.line_color, b.line_width);
            st.dash = Some(format!("{},{}", fmt_num(2. * b.line_width), fmt_num(b.line_width)));
            svg.path(&d.d, &st);
        }
        // points
        let pts: Vec<f64> = match b.boxpoints.as_str() {
            "all" => bx.points.clone(),
            "outliers" | "suspectedoutliers" => bx.outliers.clone(),
            _ => Vec::new(),
        };
        let pc = c + b.pointpos * hw;
        let mut mk = PathData::new();
        for v in pts {
            let jitter = if b.jitter > 0. { (rng.next() * 2. - 1.) * b.jitter * hw } else { 0. };
            let (x, y) = pt(pc + jitter, v);
            Symbol::CIRCLE.path(&mut mk, x, y, b.marker_radius);
        }
        svg.path(&mk.d, &Style::fill(b.marker_color));
    }
    let _ = ctx;
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn draw_image(im: &ImageCalc, xa: &Axis, ya: &Axis, svg: &mut Svg) {
    let (px0, px1) = (xa.l2p(im.x.0), xa.l2p(im.x.1));
    let (py0, py1) = (ya.l2p(im.y.0), ya.l2p(im.y.1));
    let (x, w) = (px0.min(px1), (px1 - px0).abs());
    let (y, h) = (py0.min(py1), (py1 - py0).abs());
    // Row 0 sits at y.0; flip when the axis puts y.0 at the bottom.
    let mut tr = String::new();
    if px0 > px1 {
        tr.push_str(&format!("translate({} 0) scale(-1 1) ", fmt_num(2. * x + w)));
    }
    if py0 > py1 {
        tr.push_str(&format!("translate(0 {}) scale(1 -1)", fmt_num(2. * y + h)));
    }
    let o = &mut svg.out;
    o.push_str("<image x=\"");
    crate::svg::num(o, x);
    o.push_str("\" y=\"");
    crate::svg::num(o, y);
    o.push_str("\" width=\"");
    crate::svg::num(o, w);
    o.push_str("\" height=\"");
    crate::svg::num(o, h);
    o.push_str("\" preserveAspectRatio=\"none\" image-rendering=\"optimizeSpeed\"");
    if !tr.is_empty() {
        o.push_str(" transform=\"");
        o.push_str(tr.trim());
        o.push('"');
    }
    o.push_str(" href=\"");
    crate::svg::esc(o, &im.href);
    o.push_str("\"/>\n");
}

fn draw_heatmap(h: &HeatCalc, xa: &Axis, ya: &Axis, svg: &mut Svg) {
    let rows = h.z.len();
    let cols = h.xe.len() - 1;
    let span = h.zmax - h.zmin;
    let color = |v: f64| h.scale.at((v - h.zmin) / span);
    let xs: Vec<f64> = h.xe.iter().map(|&e| xa.l2p(e)).collect();
    let ys: Vec<f64> = h.ye.iter().map(|&e| ya.l2p(e)).collect();
    let cells = rows * cols;
    let uniform = |v: &[f64]| {
        let d = v[1] - v[0];
        v.windows(2).all(|w| ((w[1] - w[0]) - d).abs() < 0.01 * d.abs().max(1e-9))
    };
    if cells > 2500 && uniform(&xs) && uniform(&ys) && h.xgap == 0. && h.ygap == 0. {
        // One pixel per cell PNG, scaled with nearest-neighbour sampling.
        let mut rgba = vec![0u8; cells * 4];
        // Image rows top to bottom.
        let flip_y = ys[0] > ys[rows];
        let flip_x = xs[0] > xs[cols];
        for r in 0..rows {
            let rr = if flip_y { rows - 1 - r } else { r };
            for c in 0..cols {
                let cc = if flip_x { cols - 1 - c } else { c };
                if let Some(Some(v)) = h.z[rr].get(cc) {
                    let col = color(*v);
                    let k = (r * cols + c) * 4;
                    rgba[k] = col.r;
                    rgba[k + 1] = col.g;
                    rgba[k + 2] = col.b;
                    rgba[k + 3] = (col.a * 255.) as u8;
                }
            }
        }
        let png = util::png_rgba(cols, rows, &rgba);
        let (x0, x1) = (xs[0].min(xs[cols]), xs[0].max(xs[cols]));
        let (y0, y1) = (ys[0].min(ys[rows]), ys[0].max(ys[rows]));
        svg.out.push_str("<image x=\"");
        crate::svg::num(&mut svg.out, x0);
        svg.out.push_str("\" y=\"");
        crate::svg::num(&mut svg.out, y0);
        svg.out.push_str("\" width=\"");
        crate::svg::num(&mut svg.out, x1 - x0);
        svg.out.push_str("\" height=\"");
        crate::svg::num(&mut svg.out, y1 - y0);
        svg.out.push_str("\" preserveAspectRatio=\"none\" image-rendering=\"optimizeSpeed\" href=\"data:image/png;base64,");
        svg.out.push_str(&util::base64_encode(&png));
        svg.out.push_str("\"/>\n");
    } else {
        let mut batches: HashMap<(u8, u8, u8, u32), PathData> = HashMap::new();
        let mut order: Vec<(u8, u8, u8, u32)> = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                let Some(Some(v)) = h.z[r].get(c) else { continue };
                let col = color(*v);
                let k = (col.r, col.g, col.b, (col.a * 1000.) as u32);
                let d = batches.entry(k).or_insert_with(|| {
                    order.push(k);
                    PathData::new()
                });
                let (x0, x1) = (xs[c].min(xs[c + 1]) + h.xgap / 2., xs[c].max(xs[c + 1]) - h.xgap / 2.);
                let (y0, y1) = (ys[r].min(ys[r + 1]) + h.ygap / 2., ys[r].max(ys[r + 1]) - h.ygap / 2.);
                d.rect(x0, y0, x1 - x0, y1 - y0);
            }
        }
        // Non-antialiased edges: adjacent cells meet without seams.
        svg.out.push_str("<g shape-rendering=\"crispEdges\">\n");
        for k in order {
            if let Some(d) = batches.get(&k) {
                svg.path(&d.d, &Style::fill(Rgba { r: k.0, g: k.1, b: k.2, a: k.3 as f32 / 1000. }));
            }
        }
        svg.group_close();
    }
    if let Some(texts) = &h.texts {
        let fs = h.text_size;
        for r in 0..rows {
            for c in 0..cols {
                let Some(txt) = texts.get(r).and_then(|row| row.get(c)) else { continue };
                if txt.is_empty() {
                    continue;
                }
                let Some(Some(v)) = h.z[r].get(c) else { continue };
                let bg = color(*v);
                let col = bg.contrast_text();
                let cx = (xs[c] + xs[c + 1]) / 2.;
                let cy = (ys[r] + ys[r + 1]) / 2.;
                svg.text(cx, cy + fs * 0.35, txt, &TextStyle::new(fs, col).anchor(Anchor::Middle));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let s = template("%{y:.1f} units, %{x}", |n, f| match n {
            "y" => Some(format::d3(f.unwrap_or(""), 3.12345)),
            "x" => Some("A".into()),
            _ => None,
        });
        assert_eq!(s, "3.1 units, A");
    }

    #[test]
    fn heatmap_bounds() {
        assert_eq!(bounds(&[0., 1., 2.], 3), vec![-0.5, 0.5, 1.5, 2.5]);
        assert_eq!(bounds(&[0., 1., 2., 3.], 3), vec![0., 1., 2., 3.]);
        assert_eq!(bounds(&[], 2), vec![-0.5, 0.5, 1.5]);
    }

    #[test]
    fn decimation_keeps_extremes() {
        let seg: Vec<(f64, f64)> = (0..10000).map(|i| (i as f64 / 50., ((i * 7919) % 101) as f64)).collect();
        let d = decimate(&seg);
        assert!(d.len() < 1000);
        let max = d.iter().map(|p| p.1).fold(0., f64::max);
        assert_eq!(max, 100.);
    }

    #[test]
    fn symbols_parse() {
        assert_eq!(Symbol::parse(&Value::from("diamond-open")), Symbol { shape: Shape::Diamond, open: true });
        assert_eq!(Symbol::parse(&Value::from(101)), Symbol { shape: Shape::Square, open: true });
        assert_eq!(Symbol::parse(&Value::from("nonsense")), Symbol::CIRCLE);
    }
}
