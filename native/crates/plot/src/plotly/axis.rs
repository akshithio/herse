//! Cartesian axes: types, data conversion, autorange, ticks and drawing.

use std::collections::HashMap;

use serde_json::Value;

use super::Ctx;
use crate::color::Rgba;
use crate::format::{self, ExpFormat};
use crate::svg::{Anchor, Run, Style, Svg, TextStyle};
use crate::text;
use crate::ticks::{self, DateDtick, LogDtick};
use crate::time;
use crate::util::{self, Datum};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisType {
    Linear,
    Log,
    Date,
    Category,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Bottom,
    Top,
    Left,
    Right,
}

/// One entry of plotly's autorange min/max arrays.
#[derive(Clone, Copy, Debug)]
pub struct Ext {
    pub val: f64,
    pub pad: f64,
    pub extrapad: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExtOpts {
    pub padded: bool,
    pub tozero: bool,
}

#[derive(Clone, Debug)]
pub struct Tick {
    /// Linear (l) coordinate.
    pub v: f64,
    pub lines: Vec<Vec<Run>>,
    /// Font scale (log minor digits are drawn smaller).
    pub scale: f64,
}

#[derive(Clone, Debug)]
pub struct Axis {
    pub name: String,
    pub id: String,
    pub letter: char,
    pub ty: AxisType,
    pub cats: Vec<String>,
    pub cat_index: HashMap<String, usize>,
    pub domain: (f64, f64),
    pub anchor: Option<String>,
    pub overlaying: Option<String>,
    pub side: Side,
    pub position: f64,
    pub matches: Option<String>,
    pub scaleanchor: Option<String>,
    pub scaleratio: f64,
    pub constrain_domain: bool,
    pub range: (f64, f64),
    /// Layout `range` (raw values; converted once the type is known).
    pub fixed_range: Option<(Option<Value>, Option<Value>)>,
    pub reversed: bool,
    pub rangemode: String,
    pub ext_min: Vec<Ext>,
    pub ext_max: Vec<Ext>,
    /// Bars force dtick >= their spacing (plotly `_minDtick`).
    pub min_dtick: Option<f64>,
    /// No gridlines by default (bar/box position axes).
    pub no_grid_default: bool,
    /// Pixel position of range.0 / range.1 along the axis.
    pub p0: f64,
    pub p1: f64,
    /// Pixel position of the axis line (cross direction).
    pub line_pos: f64,
    pub ticks: Vec<Tick>,
    /// Label rotation actually used (degrees).
    pub label_angle: f64,
    pub st: AxisStyle,
    pub exp: ExpFormat,
    pub tickround: i32,
    pub tickexponent: i32,
    pub has_data: bool,
    /// `type` was not given: decided from the data (plotly `autotype`).
    pub type_unset: bool,
    /// `showgrid` was given explicitly (layout or template).
    pub grid_explicit: bool,
    /// `autorange` was given explicitly.
    pub autorange_given: bool,
}

#[derive(Clone, Debug)]
pub struct AxisStyle {
    pub visible: bool,
    pub showgrid: bool,
    pub gridcolor: Rgba,
    pub gridwidth: f64,
    pub griddash: Option<String>,
    pub zeroline: bool,
    pub zerolinecolor: Rgba,
    pub zerolinewidth: f64,
    pub showline: bool,
    pub linecolor: Rgba,
    pub linewidth: f64,
    pub mirror: bool,
    pub ticks_out: Option<bool>,
    pub ticklen: f64,
    pub tickwidth: f64,
    pub tickcolor: Rgba,
    pub showticklabels: bool,
    pub tickfont_size: f64,
    pub tickfont_color: Rgba,
    pub tickangle: Option<f64>,
    pub tickformat: Option<String>,
    pub tickprefix: String,
    pub ticksuffix: String,
    pub nticks: Option<f64>,
    pub tickvals: Option<Vec<Value>>,
    pub ticktext: Option<Vec<String>>,
    pub dtick: Option<Value>,
    pub tick0: Option<Value>,
    pub title: String,
    pub title_size: f64,
    pub title_color: Rgba,
    pub standoff: Option<f64>,
    pub automargin: bool,
    pub categoryorder: String,
    pub categoryarray: Vec<String>,
}

impl Axis {
    pub fn new(ctx: &Ctx, name: &str) -> Axis {
        let letter = name.chars().next().unwrap_or('x');
        let id = format!("{}{}", letter, &name[5.min(name.len())..]);
        let a = |k: &str| ctx.axis_attr(name, k);
        let b = |k: &str, d: bool| a(k).and_then(|v| v.as_bool()).unwrap_or(d);
        let f = |k: &str| a(k).and_then(util::num);
        let s = |k: &str| a(k).and_then(|v| v.as_str()).map(str::to_string);
        let c = |k: &str| a(k).and_then(|v| v.as_str()).and_then(Rgba::parse);
        let theme = &ctx.theme;
        let font_color = theme.font_color;
        let dom = a("domain")
            .and_then(|v| v.as_array())
            .and_then(|d| Some((util::num(d.first()?)?, util::num(d.get(1)?)?)))
            .filter(|(a, b)| a < b)
            .unwrap_or((0., 1.));
        let side = match s("side").as_deref() {
            Some("top") if letter == 'x' => Side::Top,
            Some("right") if letter == 'y' => Side::Right,
            _ => {
                if letter == 'x' {
                    Side::Bottom
                } else {
                    Side::Left
                }
            }
        };
        let ticks_out = match s("ticks").as_deref() {
            Some("outside") => Some(true),
            Some("inside") => Some(false),
            _ => None,
        };
        let title = match a("title") {
            Some(Value::String(t)) => t.clone(),
            Some(v) => util::path(v, "text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            None => String::new(),
        };
        let tickfont_size = a("tickfont")
            .and_then(|v| util::path(v, "size"))
            .and_then(util::num)
            .unwrap_or(theme.font_size);
        let tickfont_color = a("tickfont")
            .and_then(|v| util::path(v, "color"))
            .and_then(|v| v.as_str())
            .and_then(Rgba::parse)
            .unwrap_or(font_color);
        let title_size = a("title")
            .and_then(|v| util::path(v, "font.size"))
            .and_then(util::num)
            .unwrap_or((theme.font_size * 1.2).round());
        let title_color = a("title")
            .and_then(|v| util::path(v, "font.color"))
            .and_then(|v| v.as_str())
            .and_then(Rgba::parse)
            .unwrap_or(font_color);
        let standoff = a("title").and_then(|v| util::path(v, "standoff")).and_then(util::num);
        let fixed_range = a("range").and_then(|v| v.as_array()).map(|r| {
            let g = |i: usize| r.get(i).filter(|v| !v.is_null());
            (g(0).cloned(), g(1).cloned())
        });
        let autorange = a("autorange");
        let reversed = matches!(autorange.and_then(|v| v.as_str()), Some("reversed" | "min reversed" | "max reversed"));
        let autorange_true = matches!(autorange, Some(Value::Bool(true)))
            || matches!(autorange.and_then(|v| v.as_str()), Some("reversed" | "max" | "min" | "min reversed" | "max reversed"));
        let ty = match s("type").as_deref() {
            Some("linear") => Some(AxisType::Linear),
            Some("log") => Some(AxisType::Log),
            Some("date") => Some(AxisType::Date),
            Some("category") => Some(AxisType::Category),
            _ => None,
        };
        let gridcolor = c("gridcolor").unwrap_or(theme.default_grid);
        let st = AxisStyle {
            visible: b("visible", true),
            showgrid: a("showgrid").and_then(|v| v.as_bool()).unwrap_or(true),
            gridcolor,
            gridwidth: f("gridwidth").unwrap_or(1.),
            griddash: s("griddash").and_then(|d| super::dash_array(&d, f("gridwidth").unwrap_or(1.))),
            zeroline: b("zeroline", true),
            zerolinecolor: c("zerolinecolor").unwrap_or(font_color_default(theme)),
            zerolinewidth: f("zerolinewidth").unwrap_or(1.),
            showline: b("showline", false),
            linecolor: c("linecolor").unwrap_or(font_color_default(theme)),
            linewidth: f("linewidth").unwrap_or(1.),
            mirror: matches!(a("mirror"), Some(Value::Bool(true)))
                || matches!(a("mirror").and_then(|v| v.as_str()), Some("ticks" | "all" | "allticks")),
            ticks_out,
            ticklen: f("ticklen").unwrap_or(5.),
            tickwidth: f("tickwidth").unwrap_or(1.),
            tickcolor: c("tickcolor").unwrap_or(font_color_default(theme)),
            showticklabels: b("showticklabels", true),
            tickfont_size,
            tickfont_color,
            tickangle: f("tickangle"),
            tickformat: s("tickformat").filter(|f| !f.is_empty()),
            tickprefix: s("tickprefix").unwrap_or_default(),
            ticksuffix: s("ticksuffix").unwrap_or_default(),
            nticks: f("nticks").filter(|n| *n > 0.),
            tickvals: a("tickvals").and_then(|v| v.as_array()).cloned(),
            ticktext: a("ticktext").and_then(|v| v.as_array()).map(|a| a.iter().map(util::label_of).collect()),
            dtick: a("dtick").cloned(),
            tick0: a("tick0").cloned(),
            title,
            title_size,
            title_color,
            standoff,
            automargin: !matches!(a("automargin"), Some(Value::Bool(false)) | None),
            categoryorder: s("categoryorder").unwrap_or_else(|| "trace".into()),
            categoryarray: a("categoryarray")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(util::label_of).collect())
                .unwrap_or_default(),
        };
        let mut st = st;
        // Dark app background: chrome colors adapt, data colors don't.
        st.gridcolor = theme.grid(st.gridcolor);
        st.zerolinecolor = theme.line(st.zerolinecolor);
        st.linecolor = theme.line(st.linecolor);
        st.tickcolor = theme.line(st.tickcolor);
        st.tickfont_color = theme.text(st.tickfont_color);
        st.title_color = theme.text(st.title_color);
        let exp = ExpFormat {
            format: s("exponentformat").unwrap_or_else(|| "B".into()),
            separate_thousands: b("separatethousands", false),
            minexponent: f("minexponent").map(|v| v as i32).unwrap_or(3),
        };
        Axis {
            name: name.to_string(),
            id,
            letter,
            ty: ty.unwrap_or(AxisType::Linear),
            cats: Vec::new(),
            cat_index: HashMap::new(),
            domain: dom,
            anchor: s("anchor"),
            overlaying: s("overlaying").filter(|o| o != "free"),
            side,
            position: f("position").unwrap_or(0.),
            matches: s("matches"),
            scaleanchor: s("scaleanchor"),
            scaleratio: f("scaleratio").filter(|r| *r > 0.).unwrap_or(1.),
            constrain_domain: s("constrain").as_deref() == Some("domain"),
            range: (0., 1.),
            fixed_range: if autorange_true { None } else { fixed_range },
            reversed,
            rangemode: s("rangemode").unwrap_or_else(|| "normal".into()),
            ext_min: Vec::new(),
            ext_max: Vec::new(),
            min_dtick: None,
            no_grid_default: false,
            p0: 0.,
            p1: 1.,
            line_pos: 0.,
            ticks: Vec::new(),
            label_angle: 0.,
            st,
            exp,
            tickround: 0,
            tickexponent: 0,
            has_data: false,
            type_unset: ty.is_none(),
            grid_explicit: a("showgrid").is_some(),
            autorange_given: autorange.is_some(),
        }
    }

    pub fn is_x(&self) -> bool {
        self.letter == 'x'
    }

    /// Data value -> linear coordinate (category values must be registered).
    pub fn d2l(&self, d: Datum) -> Option<f64> {
        match self.ty {
            AxisType::Linear => match d {
                Datum::Num(f) => Some(f),
                Datum::Str(s) => util::parse_num(s),
                _ => None,
            },
            AxisType::Log => {
                let v = match d {
                    Datum::Num(f) => f,
                    Datum::Str(s) => util::parse_num(s)?,
                    _ => return None,
                };
                (v > 0.).then(|| v.log10())
            }
            AxisType::Date => match d {
                Datum::Num(f) => Some(f),
                Datum::Str(s) => time::parse_datetime(s).map(|(t, _)| t).or_else(|| util::parse_num(s)),
                _ => None,
            },
            AxisType::Category => {
                let key = d.label();
                if matches!(d, Datum::Null | Datum::Other) {
                    return None;
                }
                self.cat_index.get(&key).map(|&i| i as f64)
            }
        }
    }

    /// Registers a category (in order of appearance) and returns its index.
    pub fn add_cat(&mut self, label: String) -> usize {
        if let Some(&i) = self.cat_index.get(&label) {
            return i;
        }
        let i = self.cats.len();
        self.cat_index.insert(label.clone(), i);
        self.cats.push(label);
        i
    }

    /// Converts a value given in axis data units in the layout (range, shapes).
    pub fn r2l(&self, v: &Value) -> Option<f64> {
        match self.ty {
            // Layout ranges on log axes are already in log units.
            AxisType::Log => util::num_loose(v),
            AxisType::Category => match v {
                Value::Number(_) => util::num(v),
                Value::String(s) => self.cat_index.get(s).map(|&i| i as f64).or_else(|| util::parse_num(s)),
                _ => None,
            },
            AxisType::Date => match v {
                Value::String(s) => time::parse_datetime(s).map(|(t, _)| t),
                _ => util::num(v),
            },
            AxisType::Linear => util::num_loose(v),
        }
    }

    /// Data-unit value from a layout position (annotations / shapes use data
    /// units, which on log axes are *not* logged for shapes but are for annotations).
    pub fn d2l_value(&self, v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => self.d2l(Datum::Num(n.as_f64()?)),
            Value::String(s) => self.d2l(Datum::Str(s)),
            _ => None,
        }
    }

    pub fn len(&self) -> f64 {
        (self.p1 - self.p0).abs()
    }

    pub fn l2p(&self, v: f64) -> f64 {
        let (r0, r1) = self.range;
        if r1 == r0 {
            return (self.p0 + self.p1) / 2.;
        }
        self.p0 + (v - r0) * (self.p1 - self.p0) / (r1 - r0)
    }

    pub fn in_range(&self, v: f64) -> bool {
        let (lo, hi) = if self.range.0 <= self.range.1 { self.range } else { (self.range.1, self.range.0) };
        let eps = (hi - lo) * 1e-6;
        v >= lo - eps && v <= hi + eps
    }

    /// plotly `findExtremes` for one set of values.
    pub fn expand(&mut self, vals: &[f64], ppad: &PadSpec, opts: ExtOpts) {
        let tozero = opts.tozero && self.ty == AxisType::Linear;
        let is_log = self.ty == AxisType::Log;
        let add = |v: f64, pad: f64, axis: &mut Axis| {
            if !v.is_finite() {
                return;
            }
            let (mut dmin, mut dmax) = (v, v);
            if tozero {
                dmin = dmin.min(0.);
                dmax = dmax.max(0.);
            }
            collapse(&mut axis.ext_min, dmin, pad, opts.padded, tozero, |a, b| a <= b);
            collapse(&mut axis.ext_max, dmax, pad, opts.padded, tozero, |a, b| a >= b);
        };
        let _ = is_log;
        match ppad {
            PadSpec::Const(p) => {
                // Only the extremes matter for a constant pad.
                if let Some((lo, hi)) = util::extent(vals.iter().copied()) {
                    add(lo, *p, self);
                    add(hi, *p, self);
                }
            }
            PadSpec::Each(ps) => {
                for (i, &v) in vals.iter().enumerate() {
                    add(v, ps.get(i).copied().unwrap_or(0.), self);
                }
            }
        }
    }

    /// Autorange (plotly `getAutoRange`), honoring a layout range.
    pub fn autorange(&mut self) {
        let len = self.len().max(1.);
        let default = match self.ty {
            AxisType::Date => (time::ms_from_ymd(2000, 1, 1), time::ms_from_ymd(2001, 1, 1)),
            AxisType::Log => (0., 1.),
            _ => (-1., if self.is_x() { 6. } else { 4. }),
        };
        let mut r = if self.ext_min.is_empty() || self.ext_max.is_empty() {
            default
        } else {
            get_auto_range(&self.ext_min, &self.ext_max, len, &self.rangemode)
        };
        if let Some((a, b)) = &self.fixed_range {
            // A layout range may be partial (`[null, 10]`).
            let a = a.as_ref().and_then(|v| self.r2l(v));
            let b = b.as_ref().and_then(|v| self.r2l(v));
            if a.is_some() || b.is_some() {
                let a = a.unwrap_or(if self.reversed { r.1 } else { r.0 });
                let b = b.unwrap_or(if self.reversed { r.0 } else { r.1 });
                if a.is_finite() && b.is_finite() && a != b {
                    self.range = (a, b);
                    return;
                }
            }
        }
        if !(r.0.is_finite() && r.1.is_finite()) {
            r = default;
        }
        if r.0 == r.1 {
            r = (r.0 - 1., r.1 + 1.);
        }
        self.range = if self.reversed { (r.1, r.0) } else { r };
    }

    // ------------------------------------------------------------ ticks

    /// Computes tick positions and labels for the current range / length.
    pub fn calc_ticks(&mut self) {
        self.ticks.clear();
        let (r0, r1) = self.range;
        if !(r0.is_finite() && r1.is_finite()) || r0 == r1 {
            return;
        }
        let (lo, hi) = if r0 < r1 { (r0, r1) } else { (r1, r0) };
        let is_y = !self.is_x();
        let len = self.len().max(1.);
        // tickvals / ticktext
        if let Some(vals) = self.st.tickvals.clone() {
            let texts = self.st.ticktext.clone();
            for (i, v) in vals.iter().enumerate() {
                let Some(l) = self.d2l_value(v).or_else(|| self.r2l(v)) else { continue };
                if !self.in_range(l) {
                    continue;
                }
                let label = match texts.as_ref().and_then(|t| t.get(i)) {
                    Some(t) => t.clone(),
                    None => self.auto_label(l, None).0,
                };
                self.push_tick(l, &label, 1.);
            }
            return;
        }
        match self.ty {
            AxisType::Category => {
                let n = self.cats.len();
                if n == 0 {
                    return;
                }
                let min_px = (self.st.tickfont_size * 1.2).round().max(1.);
                let nt = self.st.nticks.unwrap_or(len / min_px);
                let rough = (hi - lo) / nt.max(1.);
                let dtick = match self.st.dtick.as_ref().and_then(util::num) {
                    Some(d) if d >= 1. => d.round(),
                    _ => rough.max(1.).ceil(),
                };
                let first = lo.ceil().max(0.);
                let mut v = first;
                while v <= hi + 1e-9 && (v as usize) < n {
                    let label = self.cats[v as usize].clone();
                    self.push_tick(v, &label, 1.);
                    v += dtick;
                }
            }
            AxisType::Linear => {
                let nt = ticks::plotly_nticks(len, is_y, self.st.nticks);
                let mut dtick = ticks::plotly_linear_dtick((hi - lo) / nt);
                let mut tick0 = 0.;
                if let Some(md) = self.min_dtick
                    && md > 0.
                    && dtick < md * 2.
                {
                    dtick = md;
                }
                if let Some(d) = self.st.dtick.as_ref().and_then(util::num).filter(|d| *d > 0.) {
                    dtick = d;
                }
                if let Some(t0) = self.st.tick0.as_ref().and_then(util::num_loose) {
                    tick0 = t0;
                }
                if (hi - lo) / dtick > 500. {
                    dtick = ticks::plotly_linear_dtick((hi - lo) / 10.);
                }
                let (tr, ex) = format::plotly_tick_round(dtick, (r0, r1), &self.exp);
                self.tickround = tr;
                self.tickexponent = ex;
                for v in ticks::linear_ticks(lo, hi, dtick, tick0, 500) {
                    let label = self.auto_label(v, Some(dtick)).0;
                    self.push_tick(v, &label, 1.);
                }
            }
            AxisType::Log => {
                let nt = ticks::plotly_nticks(len, is_y, self.st.nticks);
                let rough = (hi - lo) / nt;
                let mut dtick = ticks::plotly_log_dtick((lo, hi), rough);
                match self.st.dtick.as_ref() {
                    Some(Value::Number(n)) => {
                        if let Some(d) = n.as_f64().filter(|d| *d > 0.) {
                            dtick = LogDtick::Decades(d.max(1.).round());
                        }
                    }
                    Some(Value::String(s)) if s == "D1" => dtick = LogDtick::D1,
                    Some(Value::String(s)) if s == "D2" => dtick = LogDtick::D2,
                    _ => {}
                }
                if let LogDtick::L(d) = dtick {
                    let (tr, ex) = format::plotly_tick_round(d, (10f64.powf(r0), 10f64.powf(r1)), &self.exp);
                    self.tickround = tr;
                    self.tickexponent = ex;
                }
                for v in ticks::log_ticks((lo, hi), dtick, 500) {
                    let (label, scale) = self.log_label(v, dtick);
                    self.push_tick(v, &label, scale);
                }
            }
            AxisType::Date => {
                let nt = ticks::plotly_nticks(len, is_y, self.st.nticks);
                let (mut dtick, mut tick0) = ticks::plotly_date_dtick((hi - lo) / nt);
                match self.st.dtick.as_ref() {
                    Some(Value::Number(n)) => {
                        if let Some(d) = n.as_f64().filter(|d| *d > 0.) {
                            dtick = DateDtick::Ms(d);
                        }
                    }
                    Some(Value::String(s)) => {
                        if let Some(m) = s.strip_prefix('M').and_then(|m| m.parse::<i64>().ok()).filter(|m| *m > 0) {
                            dtick = DateDtick::Months(m);
                        } else if let Some(d) = util::parse_num(s).filter(|d| *d > 0.) {
                            dtick = DateDtick::Ms(d);
                        }
                    }
                    _ => {}
                }
                if let Some(t0) = self.st.tick0.as_ref().and_then(|v| self.r2l(v)) {
                    tick0 = t0;
                }
                let round = ticks::plotly_date_round(dtick, tick0);
                let mut prev_head: Option<String> = None;
                for v in ticks::date_ticks(lo, hi, dtick, tick0, 500) {
                    let (main, head) = match &self.st.tickformat {
                        Some(f) => (time::strftime(f, v), None),
                        None => ticks::plotly_date_label(v, round),
                    };
                    let mut lines = text::rich(&format!("{}{}{}", self.st.tickprefix, main, self.st.ticksuffix));
                    if let Some(h) = head
                        && prev_head.as_deref() != Some(h.as_str())
                    {
                        lines.extend(text::rich(&h));
                        prev_head = Some(h);
                    }
                    self.ticks.push(Tick { v, lines, scale: 1. });
                }
            }
        }
    }

    fn push_tick(&mut self, v: f64, label: &str, scale: f64) {
        let full = if self.ty == AxisType::Category || self.st.tickvals.is_some() {
            label.to_string()
        } else {
            format!("{}{}{}", self.st.tickprefix, label, self.st.ticksuffix)
        };
        self.ticks.push(Tick {
            v,
            lines: text::rich(&full),
            scale,
        });
    }

    /// Label for a linear-axis value.
    fn auto_label(&self, v: f64, _dtick: Option<f64>) -> (String, f64) {
        match self.ty {
            AxisType::Category => (self.cats.get(v.round().max(0.) as usize).cloned().unwrap_or_default(), 1.),
            AxisType::Date => (time::strftime(self.st.tickformat.as_deref().unwrap_or("%b %-d, %Y"), v), 1.),
            AxisType::Log => self.log_label(v, LogDtick::L(1.)),
            AxisType::Linear => {
                if let Some(f) = &self.st.tickformat {
                    return (format::minus(format::d3(f, v)), 1.);
                }
                (format::plotly_num(v, self.tickround, self.tickexponent, &self.exp), 1.)
            }
        }
    }

    fn log_label(&self, x: f64, dtick: LogDtick) -> (String, f64) {
        let value = 10f64.powf(x);
        if let Some(f) = &self.st.tickformat {
            return (format::minus(format::d3(f, value)), 1.);
        }
        match dtick {
            LogDtick::L(_) => (format::plotly_num(value, self.tickround, self.tickexponent, &self.exp), 1.),
            _ => {
                let is_decade = (x + 0.01).rem_euclid(1.) < 0.1;
                if matches!(dtick, LogDtick::Decades(_)) || is_decade {
                    let p = x.round() as i32;
                    match self.exp.format.as_str() {
                        "power" => {
                            let t = match p {
                                0 => "1".to_string(),
                                1 => "10".to_string(),
                                _ => format!("10<sup>{}{}</sup>", if p > 1 { "" } else { "\u{2212}" }, p.abs()),
                            };
                            (t, 1.25)
                        }
                        "e" | "E" if p.abs() > 2 => {
                            (format!("1{}{}{}", self.exp.format, if p > 0 { '+' } else { '\u{2212}' }, p.abs()), 1.)
                        }
                        _ => (format::plotly_hover_num(10f64.powi(p), &self.exp), 1.),
                    }
                } else {
                    let digit = 10f64.powf(x.rem_euclid(1.)).round();
                    (format!("{digit}"), 0.75)
                }
            }
        }
    }

    /// Lays out tick labels: picks an auto angle for crowded x labels.
    pub fn choose_label_angle(&mut self) {
        if let Some(a) = self.st.tickangle {
            self.label_angle = a;
            return;
        }
        self.label_angle = 0.;
        if !self.is_x() || self.ticks.len() < 2 || !self.st.showticklabels {
            return;
        }
        let fs = self.st.tickfont_size;
        let mut min_gap = f64::INFINITY;
        let mut max_w: f64 = 0.;
        let mut overlap = false;
        for w in self.ticks.windows(2) {
            let gap = (self.l2p(w[1].v) - self.l2p(w[0].v)).abs();
            min_gap = min_gap.min(gap);
            let w0 = lines_width(&w[0].lines, fs * w[0].scale);
            let w1 = lines_width(&w[1].lines, fs * w[1].scale);
            max_w = max_w.max(w0).max(w1);
            if (w0 + w1) / 2. + 3. > gap {
                overlap = true;
            }
        }
        if !overlap {
            return;
        }
        // plotly's autotickangles: [0, 30, 90]
        let line_h = fs * 1.3;
        if min_gap >= line_h / (30f64.to_radians().sin()) && max_w * 30f64.to_radians().cos() < 300. {
            self.label_angle = 30.;
        } else {
            self.label_angle = 90.;
        }
    }

    /// Extent of the tick labels perpendicular to the axis (px), for margins.
    pub fn label_depth(&self) -> f64 {
        if !self.st.showticklabels || self.ticks.is_empty() {
            return 0.;
        }
        let fs = self.st.tickfont_size;
        let mut depth: f64 = 0.;
        for t in &self.ticks {
            let w = lines_width(&t.lines, fs * t.scale);
            let h = t.lines.len().max(1) as f64 * fs * 1.3;
            let d = if self.is_x() {
                let a = self.label_angle.to_radians();
                (w * a.sin().abs() + h * a.cos().abs()).max(h.min(fs))
            } else {
                let a = self.label_angle.to_radians();
                w * a.cos().abs() + h * a.sin().abs()
            };
            depth = depth.max(d);
        }
        depth
    }

    pub fn ticks_out_len(&self) -> f64 {
        if self.st.ticks_out == Some(true) { self.st.ticklen } else { 0. }
    }

    /// Distance from the axis line to the start of the tick labels.
    pub fn label_gap(&self) -> f64 {
        let out = self.ticks_out_len();
        if self.is_x() {
            out + if out > 0. { 1.4 } else { 0. }
        } else {
            out + 1. + if out > 0. { 1. } else { 0. }
        }
    }

    // ------------------------------------------------------------ drawing

    /// Gridlines and zero line over the counter-axis span `cross` (px).
    pub fn draw_grid(&self, svg: &mut Svg, cross: (f64, f64), has_bars_or_fill: bool) {
        if !self.st.visible {
            return;
        }
        let zl = self.show_zeroline(has_bars_or_fill);
        let showgrid = self.st.showgrid && !self.no_grid_default_hidden();
        if showgrid && self.st.gridwidth > 0. {
            let mut d = crate::svg::PathData::new();
            for t in &self.ticks {
                if zl && t.v.abs() < 1e-12 {
                    continue;
                }
                let p = self.l2p(t.v);
                // plotly omits gridlines that sit on the plot edge when a line is drawn there
                if self.is_x() {
                    d.move_to(crisp(p), cross.0);
                    d.line_to(crisp(p), cross.1);
                } else {
                    d.move_to(cross.0, crisp(p));
                    d.line_to(cross.1, crisp(p));
                }
            }
            let mut style = Style::stroke(self.st.gridcolor, self.st.gridwidth);
            style.dash = self.st.griddash.clone();
            svg.path(&d.d, &style);
        }
        if zl {
            let p = self.l2p(0.);
            let style = Style::stroke(self.st.zerolinecolor, self.st.zerolinewidth);
            if self.is_x() {
                svg.line(crisp(p), cross.0, crisp(p), cross.1, &style);
            } else {
                svg.line(cross.0, crisp(p), cross.1, crisp(p), &style);
            }
        }
    }

    fn no_grid_default_hidden(&self) -> bool {
        // Bar/box position axes have no grid unless asked for explicitly.
        self.no_grid_default && !self.grid_explicit
    }

    pub fn show_zeroline(&self, has_bars_or_fill: bool) -> bool {
        if !self.st.zeroline || self.ty != AxisType::Linear || !self.st.visible {
            return false;
        }
        let (r0, r1) = self.range;
        if r0 * r1 > 0. {
            return false;
        }
        let p = self.l2p(0.);
        let at_edge = (p - self.p0).abs() < 1. || (p - self.p1).abs() < 1.;
        !at_edge || has_bars_or_fill || !self.st.showline
    }

    /// Axis line, tick marks, labels and title.
    pub fn draw(&self, svg: &mut Svg, mirror_pos: Option<f64>, ctx: &Ctx) -> f64 {
        if !self.st.visible {
            return 0.;
        }
        let st = &self.st;
        let lp = self.line_pos;
        let sign = match self.side {
            Side::Bottom | Side::Right => 1.,
            Side::Top | Side::Left => -1.,
        };
        if st.showline {
            let style = Style::stroke(st.linecolor, st.linewidth);
            let (a, b) = (self.p0.min(self.p1), self.p0.max(self.p1));
            for pos in std::iter::once(lp).chain(mirror_pos.filter(|_| st.mirror)) {
                if self.is_x() {
                    svg.line(a - st.linewidth / 2., crisp(pos), b + st.linewidth / 2., crisp(pos), &style);
                } else {
                    svg.line(crisp(pos), a, crisp(pos), b, &style);
                }
            }
        }
        if let Some(out) = st.ticks_out {
            let dir = if out { sign } else { -sign };
            let mut d = crate::svg::PathData::new();
            for t in &self.ticks {
                let p = crisp(self.l2p(t.v));
                if self.is_x() {
                    d.move_to(p, lp);
                    d.line_to(p, lp + dir * st.ticklen);
                } else {
                    d.move_to(lp, p);
                    d.line_to(lp + dir * st.ticklen, p);
                }
            }
            svg.path(&d.d, &Style::stroke(st.tickcolor, st.tickwidth));
        }
        let mut depth = self.label_gap();
        if st.showticklabels {
            let fs = st.tickfont_size;
            let gap = self.label_gap();
            let mut max_depth: f64 = 0.;
            for t in &self.ticks {
                let p = self.l2p(t.v);
                let size = fs * t.scale;
                let style = TextStyle::new(size, st.tickfont_color);
                let w = lines_width(&t.lines, size);
                let nl = t.lines.len();
                if self.is_x() {
                    let ang = self.label_angle;
                    if ang == 0. {
                        let base = if sign > 0. { lp + gap + fs * 1.083 } else { lp - gap - fs * 0.25 - (nl as f64 - 1.) * fs * 1.3 };
                        for (k, line) in t.lines.iter().enumerate() {
                            svg.text_runs(p, base + k as f64 * fs * 1.3, line, &style.clone().anchor(Anchor::Middle));
                        }
                        max_depth = max_depth.max(fs * 1.3 * nl as f64);
                    } else {
                        // Rotated (plotly convention: positive = clockwise),
                        // hanging off the axis from the tick.
                        let th = ang.to_radians();
                        let line = t.lines.iter().flatten().cloned().collect::<Vec<_>>();
                        let hanging_start = (ang > 0.) == (sign > 0.);
                        let anchor = if hanging_start { Anchor::Start } else { Anchor::End };
                        let x = p - 0.35 * fs * th.sin();
                        let y = lp + sign * (gap + 2. + 0.7 * fs * th.cos().abs()) + 0.35 * fs * th.cos() - 0.35 * fs * th.cos().abs() * if sign > 0. { 0. } else { 1. };
                        svg.text_runs(x, y, &line, &style.clone().anchor(anchor).rotate(ang));
                        max_depth = max_depth.max(text::runs_width(&line, size) * th.sin().abs() + fs * th.cos().abs() + 2.);
                    }
                } else {
                    let x = if sign < 0. { lp - gap } else { lp + gap };
                    let anchor = if sign < 0. { Anchor::End } else { Anchor::Start };
                    let total_h = (nl as f64 - 1.) * fs * 1.3;
                    for (k, line) in t.lines.iter().enumerate() {
                        let y = p + fs * 0.35 - total_h / 2. + k as f64 * fs * 1.3;
                        svg.text_runs(x, y, line, &style.clone().anchor(anchor));
                    }
                    max_depth = max_depth.max(w);
                }
            }
            depth = gap + max_depth;
        }
        let _ = ctx;
        depth
    }

    /// Draws the axis title `depth` px away from the axis line.
    pub fn draw_title(&self, svg: &mut Svg, depth: f64, mid: f64) {
        if self.st.title.is_empty() || !self.st.visible {
            return;
        }
        let st = &self.st;
        let lines = text::rich(&st.title);
        let fs = st.title_size;
        let standoff = st.standoff.unwrap_or(15.);
        let style = TextStyle::new(fs, st.title_color).anchor(Anchor::Middle);
        let n = lines.len() as f64;
        match self.side {
            Side::Bottom => {
                let mut y = self.line_pos + depth + 0.22 * st.tickfont_size + standoff + 0.7 * fs;
                for line in &lines {
                    svg.text_runs(mid, y, line, &style);
                    y += fs * 1.3;
                }
            }
            Side::Top => {
                let mut y = self.line_pos - depth - standoff - 0.3 * fs - (n - 1.) * fs * 1.3;
                for line in &lines {
                    svg.text_runs(mid, y, line, &style);
                    y += fs * 1.3;
                }
            }
            Side::Left => {
                let mut x = self.line_pos - depth - standoff - 0.43 * fs - (n - 1.) * fs * 1.3;
                for line in &lines {
                    svg.text_runs(x, mid, line, &style.clone().rotate(-90.));
                    x += fs * 1.3;
                }
            }
            Side::Right => {
                let mut x = self.line_pos + depth + standoff + 0.75 * fs;
                for line in &lines {
                    svg.text_runs(x, mid, line, &style.clone().rotate(-90.));
                    x += fs * 1.3;
                }
            }
        }
    }

    /// Space the title needs beyond the labels.
    pub fn title_depth(&self) -> f64 {
        if self.st.title.is_empty() || !self.st.visible {
            return 0.;
        }
        let n = text::rich(&self.st.title).len() as f64;
        let standoff = self.st.standoff.unwrap_or(15.);
        standoff + self.st.title_size * (1. + 1.3 * (n - 1.)) + 0.22 * self.st.tickfont_size
    }
}

pub fn lines_width(lines: &[Vec<Run>], size: f64) -> f64 {
    lines.iter().map(|l| text::runs_width(l, size)).fold(0., f64::max)
}

fn font_color_default(theme: &super::Theme) -> Rgba {
    theme.axis_default
}

/// Snap a 1px line to the pixel grid like plotly's `crisp` class does.
pub fn crisp(p: f64) -> f64 {
    (p - 0.5).round() + 0.5
}

#[derive(Clone, Debug)]
pub enum PadSpec {
    Const(f64),
    Each(Vec<f64>),
}

fn collapse(arr: &mut Vec<Ext>, val: f64, pad: f64, extrapad: bool, tozero: bool, at_least: impl Fn(f64, f64) -> bool) {
    let mut include = true;
    let mut j = 0;
    while j < arr.len() {
        let v = arr[j];
        if at_least(v.val, val) && v.pad >= pad && (v.extrapad || !extrapad) {
            include = false;
            break;
        } else if at_least(val, v.val) && v.pad <= pad && (extrapad || !v.extrapad) {
            arr.remove(j);
            continue;
        }
        j += 1;
    }
    if include {
        let clip = tozero && val == 0.;
        arr.push(Ext {
            val,
            pad: if clip { 0. } else { pad },
            extrapad: if clip { false } else { extrapad },
        });
    }
}

fn get_auto_range(min: &[Ext], max: &[Ext], len: f64, rangemode: &str) -> (f64, f64) {
    let extrappad = 0.05 * len;
    let pad = |e: &Ext| if e.pad.is_nan() { 0. } else { e.pad + if e.extrapad { extrappad } else { 0. } };
    let mut minmin = min[0].val;
    let mut maxmax = max[0].val;
    for e in &min[1..] {
        if minmin != maxmax {
            break;
        }
        minmin = minmin.min(e.val);
    }
    for e in &max[1..] {
        if minmin != maxmax {
            break;
        }
        maxmax = maxmax.max(e.val);
    }
    let to_zero = rangemode == "tozero";
    let non_neg = rangemode == "nonnegative";
    let min_span = len / 10.;
    let mut mbest = 0.;
    let mut minbest = min[0];
    let mut maxbest = max[0];
    let mut nopad_min = false;
    let mut nopad_max = false;
    for mn in min {
        for mx in max {
            let dv = mx.val - mn.val;
            if dv > 0. {
                let dp = len - pad(mn) - pad(mx);
                if dp > min_span {
                    if dv / dp > mbest {
                        minbest = *mn;
                        maxbest = *mx;
                        nopad_min = false;
                        nopad_max = false;
                        mbest = dv / dp;
                    }
                } else if dv / len > mbest {
                    minbest = *mn;
                    maxbest = *mx;
                    nopad_min = true;
                    nopad_max = true;
                    mbest = dv / len;
                }
            }
        }
    }
    let gp = |e: &Ext, nopad: bool| if nopad { 0. } else { pad(e) };
    if minmin == maxmax {
        let lower = minmin - 1.;
        let upper = minmin + 1.;
        if to_zero {
            if minmin == 0. {
                return (0., 1.);
            }
            let arr = if minmin > 0. { max } else { min };
            let max_pad = arr.iter().map(pad).fold(0., f64::max);
            let end = minmin / (1. - (max_pad / len).min(0.5));
            return if minmin > 0. { (0., end) } else { (end, 0.) };
        } else if non_neg {
            return (lower.max(0.), upper.max(1.));
        }
        return (lower, upper);
    }
    if to_zero {
        if minbest.val >= 0. {
            minbest = Ext { val: 0., pad: 0., extrapad: false };
            nopad_min = true;
        }
        if maxbest.val <= 0. {
            maxbest = Ext { val: 0., pad: 0., extrapad: false };
            nopad_max = true;
        }
    } else if non_neg {
        if minbest.val - mbest * gp(&minbest, nopad_min) < 0. {
            minbest = Ext { val: 0., pad: 0., extrapad: false };
            nopad_min = true;
        }
        if maxbest.val <= 0. {
            maxbest = Ext { val: 1., pad: 0., extrapad: false };
            nopad_max = true;
        }
    }
    let denom = len - gp(&minbest, nopad_min) - gp(&maxbest, nopad_max);
    let m = if denom > 0. { (maxbest.val - minbest.val) / denom } else { (maxbest.val - minbest.val) / len };
    (minbest.val - m * gp(&minbest, nopad_min), maxbest.val + m * gp(&maxbest, nopad_max))
}

/// plotly `autoType` over a sample of the axis data.
pub fn auto_type(vals: &[Datum], strict_numbers: bool) -> AxisType {
    if vals.is_empty() {
        return AxisType::Linear;
    }
    let step = (vals.len() as f64 / 1000.).max(1.);
    let mut seen = std::collections::HashSet::new();
    // plotly counts dates and "isNumeric" values for moreDates(), then
    // categories vs numbers (numeric strings are numbers unless strict).
    let (mut dates, mut numeric, mut cats, mut nums) = (0usize, 0usize, 0usize, 0usize);
    let mut f = 0.;
    while (f as usize) < vals.len() {
        let d = vals[f as usize];
        f += step;
        let key = match d {
            Datum::Num(x) => format!("n{x}"),
            Datum::Str(s) => format!("s{s}"),
            Datum::Bool(b) => format!("b{b}"),
            _ => continue,
        };
        if !seen.insert(key) {
            continue;
        }
        match d {
            Datum::Num(_) => {
                numeric += 1;
                nums += 1;
            }
            Datum::Str(s) => {
                if time::is_date_string(s) {
                    dates += 1;
                }
                let is_num = util::parse_num(s).is_some();
                if is_num {
                    numeric += 1;
                }
                if is_num && !strict_numbers {
                    nums += 1;
                } else {
                    cats += 1;
                }
            }
            Datum::Bool(_) => cats += 1,
            _ => {}
        }
    }
    if dates > numeric * 2 {
        return AxisType::Date;
    }
    if cats > nums * 2 {
        return AxisType::Category;
    }
    AxisType::Linear
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: &[(f64, f64, bool)], max: &[(f64, f64, bool)], len: f64) -> (f64, f64) {
        let e = |v: &[(f64, f64, bool)]| v.iter().map(|&(val, pad, extrapad)| Ext { val, pad, extrapad }).collect::<Vec<_>>();
        get_auto_range(&e(min), &e(max), len, "normal")
    }

    #[test]
    fn bar_autorange_is_zero_based_with_5pct_headroom() {
        let r = range(&[(0., 0., false)], &[(9., 0., true)], 220.);
        assert_eq!(r.0, 0.);
        assert!((r.1 - 9.473684).abs() < 1e-4, "{r:?}");
    }

    #[test]
    fn scatter_autorange_pads_both_ends() {
        // markers of size 6 (ppad 3.75) + 5% extrapad on a 400 px axis
        let r = range(&[(0., 3.75, true)], &[(10., 3.75, true)], 400.);
        let pad_px = 3.75 + 20.;
        let m = 10. / (400. - 2. * pad_px);
        assert!((r.0 + m * pad_px).abs() < 1e-9);
        assert!((r.1 - 10. - m * pad_px).abs() < 1e-9);
    }

    #[test]
    fn autotype() {
        fn d<'a>(v: &[&'a str]) -> Vec<Datum<'a>> {
            v.iter().map(|s| Datum::Str(s)).collect()
        }
        assert_eq!(auto_type(&d(&["a", "b", "c"]), false), AxisType::Category);
        assert_eq!(auto_type(&d(&["2020-01-01", "2020-02-01"]), false), AxisType::Date);
        assert_eq!(auto_type(&d(&["1", "2", "3"]), false), AxisType::Linear);
        assert_eq!(auto_type(&d(&["2019", "2020", "2021"]), true), AxisType::Category);
        assert_eq!(auto_type(&[Datum::Num(1.), Datum::Num(2.)], true), AxisType::Linear);
    }
}
