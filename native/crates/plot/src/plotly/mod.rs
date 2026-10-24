//! Plotly figure -> SVG.
//!
//! Mirrors plotly.js's pipeline at a small scale: supply defaults (layout,
//! template, colorway), autotype axes, compute per-trace "calc" data
//! (binning, stacking, box stats), autorange with plotly's padding rules,
//! auto ticks, then draw in plotly's layer order.

mod axis;
mod legend;
mod pie;
mod traces;

use std::collections::HashMap;

use serde_json::Value;

use crate::color::Rgba;
use crate::svg::{Anchor, Style, Svg, TextStyle};
use crate::text;
use crate::util::{self, Arr, Datum};
use crate::{RenderOptions, Rendered};
use axis::{Axis, AxisType, Side};
use traces::Calc;

static NULL: Value = Value::Null;

/// plotly.js's colorway when no template provides one.
const PLOTLY_JS_COLORWAY: [u32; 10] = [
    0x1f77b4, 0xff7f0e, 0x2ca02c, 0xd62728, 0x9467bd, 0x8c564b, 0xe377c2, 0x7f7f7f, 0xbcbd22, 0x17becf,
];

#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    /// Light mode, blended into the page (see `RenderOptions::blend`).
    pub blend: bool,
    /// The figure itself is dark (e.g. `plotly_dark`): keep its colors.
    pub fig_dark: bool,
    pub font_color: Rgba,
    pub font_size: f64,
    pub paper_bg: Rgba,
    pub plot_bg: Rgba,
    /// Original (light-mode) backgrounds, to recognize bar outlines that
    /// imitate the background.
    pub orig_bgs: [Rgba; 2],
    pub default_grid: Rgba,
    pub axis_default: Rgba,
    pub colorway: Vec<Rgba>,
}

impl Theme {
    /// Text-ish colors (labels, titles) for the current mode.
    pub fn text(&self, c: Rgba) -> Rgba {
        if self.dark && !self.fig_dark && c.luminance() < 0.45 {
            Rgba { a: c.a, ..Rgba::rgb(0xd7, 0xdb, 0xe2) }
        } else {
            c
        }
    }
    /// Gridline colors.
    pub fn grid(&self, c: Rgba) -> Rgba {
        if self.dark && !self.fig_dark {
            Rgba::WHITE.with_alpha(if c.luminance() > 0.6 { 0.12 } else { 0.22 } * c.a as f64)
        } else if self.blend && !self.fig_dark && c.luminance() > 0.6 {
            // Template grids are white on a tinted plot; on the page, ink.
            INK.with_alpha(0.09 * c.a as f64)
        } else {
            c
        }
    }
    /// Axis lines, zero lines, tick marks.
    pub fn line(&self, c: Rgba) -> Rgba {
        if self.dark && !self.fig_dark {
            Rgba::WHITE.with_alpha(if c.luminance() > 0.6 { 0.3 } else { 0.45 } * c.a as f64)
        } else if self.blend && !self.fig_dark && c.luminance() > 0.6 {
            INK.with_alpha(0.28 * c.a as f64)
        } else {
            c
        }
    }
    /// Marker / bar outlines: background-colored outlines vanish on dark
    /// and on a blended page.
    pub fn outline(&self, c: Rgba) -> Option<Rgba> {
        if (self.dark || self.blend) && !self.fig_dark && (self.orig_bgs.contains(&c) || c.luminance() > 0.85) {
            None
        } else {
            Some(c)
        }
    }
}

pub struct Ctx<'a> {
    pub layout: &'a Value,
    pub tl: Option<&'a Value>,
    pub theme: Theme,
    pub opts: &'a RenderOptions,
}

impl<'a> Ctx<'a> {
    /// Layout attribute, falling back to the template's layout.
    pub fn lay(&self, p: &str) -> Option<&'a Value> {
        util::path(self.layout, p).or_else(|| self.tl.and_then(|t| util::path(t, p)))
    }

    pub fn lay_num(&self, p: &str) -> Option<f64> {
        self.lay(p).and_then(util::num)
    }

    pub fn lay_str(&self, p: &str) -> Option<&'a str> {
        self.lay(p).and_then(|v| v.as_str())
    }

    pub fn lay_color(&self, p: &str) -> Option<Rgba> {
        self.lay_str(p).and_then(Rgba::parse)
    }

    /// Axis attribute: `layout.<name>.<key>`, then `template.layout.<x|y>axis.<key>`.
    pub fn axis_attr(&self, name: &str, key: &str) -> Option<&'a Value> {
        let own = self.layout.get(name).and_then(|a| util::path(a, key));
        own.or_else(|| {
            let letter = &name[..1.min(name.len())];
            self.tl
                .and_then(|t| t.get(format!("{letter}axis")))
                .and_then(|a| util::path(a, key))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Scatter,
    Bar,
    Histogram,
    Box,
    Heatmap,
    Histogram2d,
    Image,
    Candlestick,
    Ohlc,
    Pie,
}

pub struct Trace<'a> {
    pub idx: usize,
    pub raw: &'a Value,
    pub tmpl: Option<&'a Value>,
    pub kind: Kind,
    pub name: String,
    pub color: Rgba,
    pub xa: usize,
    pub ya: usize,
    pub legend_only: bool,
    pub showlegend: bool,
    pub legendgroup: String,
    pub opacity: f64,
    pub horizontal: bool,
    pub calc: Calc,
}

impl<'a> Trace<'a> {
    pub fn get(&self, p: &str) -> Option<&'a Value> {
        util::path(self.raw, p).or_else(|| self.tmpl.and_then(|t| util::path(t, p)))
    }
    pub fn num(&self, p: &str) -> Option<f64> {
        self.get(p).and_then(util::num)
    }
    pub fn str(&self, p: &str) -> Option<&'a str> {
        self.get(p).and_then(|v| v.as_str())
    }
    pub fn color_attr(&self, p: &str) -> Option<Rgba> {
        self.str(p).and_then(Rgba::parse)
    }
    pub fn arr(&self, p: &str) -> Option<Arr<'a>> {
        util::path(self.raw, p).and_then(Arr::from)
    }
}

/// SVG dash array for a plotly dash name at `width`.
pub fn dash_array(dash: &str, width: f64) -> Option<String> {
    let d = width.max(3.);
    let f = |v: &[f64]| Some(v.iter().map(|x| crate::svg::fmt_num(x * d)).collect::<Vec<_>>().join(","));
    match dash {
        "solid" | "" => None,
        "dot" => f(&[1., 1.]),
        "dash" => f(&[3., 3.]),
        "longdash" => f(&[5., 5.]),
        "dashdot" => f(&[3., 1., 1., 1.]),
        "longdashdot" => f(&[5., 2., 1., 2.]),
        other => {
            // "5px,10px,2px" or "5,10"
            let parts: Vec<f64> = other
                .split([',', ' '])
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.trim_end_matches('%').trim_end_matches("px").parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.)
                .collect();
            if parts.is_empty() || parts.iter().all(|v| *v == 0.) {
                None
            } else {
                Some(parts.iter().map(|x| crate::svg::fmt_num(*x)).collect::<Vec<_>>().join(","))
            }
        }
    }
}

/// Plot-area geometry.
#[derive(Clone, Copy, Debug)]
pub struct Area {
    pub l: f64,
    pub t: f64,
    pub w: f64,
    pub h: f64,
}

impl Area {
    pub fn r(&self) -> f64 {
        self.l + self.w
    }
    pub fn b(&self) -> f64 {
        self.t + self.h
    }
    /// paper x fraction -> px
    pub fn px(&self, x: f64) -> f64 {
        self.l + x * self.w
    }
    /// paper y fraction -> px
    pub fn py(&self, y: f64) -> f64 {
        self.t + (1. - y) * self.h
    }
}

pub(crate) fn render(fig: &Value, opts: &RenderOptions) -> Option<Rendered> {
    let obj = fig.as_object()?;
    let data: &[Value] = match obj.get("data") {
        Some(Value::Array(a)) => a,
        None | Some(Value::Null) => &[],
        _ => return None,
    };
    let layout = obj.get("layout").filter(|l| l.is_object()).unwrap_or(&NULL);
    if data.len() > 2000 {
        return None;
    }
    let mut notes = Vec::new();

    let template = layout.get("template");
    let tl = template.and_then(|t| t.get("layout")).filter(|v| v.is_object());
    let td = template.and_then(|t| t.get("data")).filter(|v| v.is_object());
    let mut ctx = Ctx {
        layout,
        tl,
        theme: Theme {
            dark: opts.dark,
            blend: opts.blend && !opts.dark,
            fig_dark: false,
            font_color: Rgba::hex_rgb(0x444444),
            font_size: 12.,
            paper_bg: Rgba::WHITE,
            plot_bg: Rgba::WHITE,
            orig_bgs: [Rgba::WHITE, Rgba::WHITE],
            default_grid: Rgba::hex_rgb(0xeeeeee),
            axis_default: Rgba::hex_rgb(0x444444),
            colorway: Vec::new(),
        },
        opts,
    };
    // Layout features we can't draw faithfully.
    for key in ["geo", "mapbox", "map", "polar", "ternary", "scene", "smith"] {
        let used = data.iter().any(|t| t.get("subplot").or_else(|| t.get(key)).is_some());
        if used {
            return None;
        }
    }
    if layout.get("grid").is_some() {
        // Subplot domains computed by plotly's grid: only fine if axes carry domains.
        let has_domains = layout
            .as_object()
            .map(|o| o.iter().filter(|(k, _)| is_axis_key(k)).all(|(_, a)| a.get("domain").is_some()))
            .unwrap_or(true);
        if !has_domains {
            return None;
        }
    }
    resolve_theme(&mut ctx);

    // ---------------------------------------------------------- traces
    let mut traces: Vec<Trace> = Vec::with_capacity(data.len());
    let mut type_counts: HashMap<String, usize> = HashMap::new();
    let mut axis_names: Vec<String> = Vec::new();
    let mut has_cartesian = false;
    let mut has_pie = false;
    for (i, t) in data.iter().enumerate() {
        if !t.is_object() {
            continue;
        }
        let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("scatter");
        let kind = match ty {
            "scatter" | "scattergl" => Kind::Scatter,
            "bar" => Kind::Bar,
            "histogram" => Kind::Histogram,
            "box" => Kind::Box,
            "heatmap" | "heatmapgl" => Kind::Heatmap,
            "histogram2d" => Kind::Histogram2d,
            "image" => Kind::Image,
            "candlestick" => Kind::Candlestick,
            "ohlc" => Kind::Ohlc,
            "pie" => Kind::Pie,
            _ => return None,
        };
        let type_key = if ty == "scattergl" { "scattergl" } else { ty };
        let n = type_counts.entry(type_key.to_string()).or_insert(0);
        let tmpl = td.and_then(|d| d.get(type_key)).and_then(|a| a.as_array()).filter(|a| !a.is_empty()).map(|a| &a[*n % a.len()]);
        *n += 1;
        let visible = t.get("visible");
        if matches!(visible, Some(Value::Bool(false))) {
            continue;
        }
        let legend_only = visible.and_then(|v| v.as_str()) == Some("legendonly");
        if t.get("transforms").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty()) {
            return None;
        }
        let name = match t.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => util::label_of(&Value::Number(n.clone())),
            _ => format!("trace {i}"),
        };
        let cw = &ctx.theme.colorway;
        let color = cw[i % cw.len()];
        let horizontal = match t.get("orientation").and_then(|v| v.as_str()) {
            Some("h") => true,
            Some(_) => false,
            None => match kind {
                // plotly infers orientation from which data array is given.
                Kind::Bar | Kind::Box => {
                    let has = |k: &str| t.get(k).map(|v| !v.is_null()).unwrap_or(false);
                    if kind == Kind::Box { has("x") && !has("y") } else { has("x") && !has("y") && false }
                }
                Kind::Histogram => {
                    let has = |k: &str| t.get(k).map(|v| !v.is_null()).unwrap_or(false);
                    has("y") && !has("x")
                }
                _ => false,
            },
        };
        let (xa_name, ya_name) = if kind == Kind::Pie {
            has_pie = true;
            (String::new(), String::new())
        } else {
            has_cartesian = true;
            let xa = t.get("xaxis").and_then(|v| v.as_str()).unwrap_or("x");
            let ya = t.get("yaxis").and_then(|v| v.as_str()).unwrap_or("y");
            (axis_name(xa, 'x')?, axis_name(ya, 'y')?)
        };
        for n in [&xa_name, &ya_name] {
            if !n.is_empty() && !axis_names.contains(n) {
                axis_names.push(n.clone());
            }
        }
        let showlegend = t.get("showlegend").and_then(|v| v.as_bool()).unwrap_or(!matches!(kind, Kind::Heatmap | Kind::Histogram2d | Kind::Image));
        let opacity = t.get("opacity").and_then(util::num).unwrap_or(1.).clamp(0., 1.);
        traces.push(Trace {
            idx: i,
            raw: t,
            tmpl,
            kind,
            name,
            color,
            xa: usize::MAX,
            ya: usize::MAX,
            legend_only,
            showlegend,
            legendgroup: t.get("legendgroup").map(util::label_of).unwrap_or_default(),
            opacity,
            horizontal,
            calc: Calc::None,
        });
        // Remember axis names per trace (resolved to indices below).
        let tr = traces.last_mut()?;
        tr.calc = Calc::Pending(xa_name, ya_name);
    }
    // Axes from the layout, too (empty subplots are still drawn).
    if let Some(o) = layout.as_object() {
        let mut keys: Vec<&String> = o.keys().filter(|k| is_axis_key(k)).collect();
        keys.sort_by_key(|k| (k.as_bytes()[0], k[5..].parse::<u32>().unwrap_or(1)));
        for k in keys {
            if !axis_names.contains(k) {
                axis_names.push(k.clone());
                has_cartesian = true;
            }
        }
    }
    if !has_cartesian && !has_pie {
        axis_names.push("xaxis".into());
        axis_names.push("yaxis".into());
        has_cartesian = true;
    }
    if !has_cartesian {
        axis_names.clear();
    }
    let mut axes: Vec<Axis> = axis_names.iter().map(|n| Axis::new(&ctx, n)).collect();
    // Overlaying axes share their base axis's domain.
    for i in 0..axes.len() {
        if let Some(base) = axes[i].overlaying.clone()
            && let Some(bi) = axes.iter().position(|a| a.id == base)
        {
            axes[i].domain = axes[bi].domain;
        }
    }
    let axis_index: HashMap<String, usize> = axes.iter().enumerate().map(|(i, a)| (a.name.clone(), i)).collect();
    for t in traces.iter_mut() {
        if let Calc::Pending(xn, yn) = &t.calc
            && t.kind != Kind::Pie {
                t.xa = *axis_index.get(xn)?;
                t.ya = *axis_index.get(yn)?;
            }
        t.calc = Calc::None;
    }
    // Every axis must be resolvable for an axis line / anchor.
    for a in &axes {
        if a.st.tickformat.as_deref().is_some_and(|f| f.contains("%{")) {
            // plotly-only date extensions in a numeric format: fine to ignore.
        }
        if ctx.axis_attr(&a.name, "rangebreaks").and_then(|v| v.as_array()).is_some_and(|v| !v.is_empty()) {
            return None;
        }
        // Finance traces turn plotly's range slider on by default.
        let finance = traces.iter().any(|t| matches!(t.kind, Kind::Candlestick | Kind::Ohlc) && axis_names.get(t.xa) == Some(&a.name));
        let slider = ctx.axis_attr(&a.name, "rangeslider.visible").and_then(|v| v.as_bool()).unwrap_or(finance);
        if slider && a.is_x() {
            push_note(&mut notes, "range slider omitted");
        }
    }
    if layout.get("updatemenus").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty())
        || layout.get("sliders").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty())
    {
        if fig.get("frames").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty())
            || layout.get("sliders").is_some()
        {
            push_note(&mut notes, "animation shown at its first frame");
        } else {
            push_note(&mut notes, "buttons and menus omitted");
        }
    }
    if layout.get("images").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty()) {
        push_note(&mut notes, "layout images omitted");
    }

    // ---------------------------------------------------------- calc
    traces::autotype_axes(&ctx, &traces, &mut axes);
    traces::register_categories(&traces, &mut axes);
    for a in axes.iter_mut() {
        traces::order_categories(a);
    }
    let calcs: Vec<Calc> = traces.iter().map(|t| traces::calc(&ctx, t, &mut axes, &mut notes)).collect::<Option<_>>()?;
    for (t, c) in traces.iter_mut().zip(calcs) {
        t.calc = c;
    }
    traces::cross_trace(&ctx, &mut traces, &mut axes);

    // ---------------------------------------------------------- geometry
    let lay_w = layout.get("width").and_then(util::num).filter(|w| *w >= 10.);
    let width = match lay_w {
        Some(w) => w.min(opts.width as f64),
        None => opts.width as f64,
    };
    let height = opts
        .height
        .map(|h| h as f64)
        .or_else(|| layout.get("height").and_then(util::num).filter(|h| *h >= 10.))
        .unwrap_or(400.)
        .clamp(80., 8000.);
    let margin = |k: &str, d: f64| ctx.lay_num(&format!("margin.{k}")).filter(|v| *v >= 0.).unwrap_or(d);
    let mut m = [margin("l", 80.), margin("r", 80.), margin("t", 100.), margin("b", 80.)];
    // Keep at least a 30% plot area on small sizes.
    let fit = |m: &mut [f64; 4]| {
        let hsum = m[0] + m[1];
        if width - hsum < width * 0.3 {
            let f = width * 0.7 / hsum.max(1.);
            m[0] *= f;
            m[1] *= f;
        }
        let vsum = m[2] + m[3];
        if height - vsum < height * 0.3 {
            let f = height * 0.7 / vsum.max(1.);
            m[2] *= f;
            m[3] *= f;
        }
    };
    fit(&mut m);

    let legend = legend::build(&ctx, &traces);
    let colorbars = traces::colorbars(&ctx, &traces);
    let base_r = m[1];
    let mut area;
    let mut iteration = 0;
    loop {
        // Legend / colorbars push the right (or bottom/top) margin.
        let mut right_extra = 0.;
        for cb in &colorbars {
            right_extra += cb.width();
        }
        let pw0 = width - m[0] - base_r;
        if let Some(lg) = &legend
            && lg.outside_right()
        {
            right_extra += lg.width + 12.;
        }
        if right_extra > 0. {
            // plot width p satisfies l + 1.02 p + extra = W
            let p = (width - m[0] - right_extra) / 1.02;
            if p < pw0 {
                m[1] = (width - m[0] - p).max(base_r);
            }
        }
        if let Some(lg) = &legend {
            let (top, bottom) = lg.vertical_push(height - m[2] - m[3]);
            m[2] = m[2].max(top);
            m[3] = m[3].max(bottom);
        }
        fit(&mut m);
        area = Area {
            l: m[0],
            t: m[2],
            w: (width - m[0] - m[1]).max(10.),
            h: (height - m[2] - m[3]).max(10.),
        };
        place_axes(&mut axes, &area);
        autorange_all(&mut axes);
        constrain(&mut axes);
        place_lines(&mut axes, &area);
        for a in axes.iter_mut() {
            a.calc_ticks();
            a.choose_label_angle();
        }
        // automargin: grow margins for labels that don't fit.
        let need = needed_margins(&axes, &area);
        let mut grew = false;
        for k in 0..4 {
            if need[k] > m[k] + 0.5 {
                m[k] = need[k];
                grew = true;
            }
        }
        iteration += 1;
        if !grew || iteration >= 3 {
            break;
        }
        fit(&mut m);
    }

    #[cfg(test)]
    tests::record(&axes, &traces);

    // ---------------------------------------------------------- draw
    let font_size = ctx.theme.font_size;
    let mut svg = Svg::new(width, height, &opts.font_family, font_size);
    if ctx.theme.paper_bg.a > 0. {
        svg.fill_rect(0., 0., width, height, ctx.theme.paper_bg);
    }
    // Subplots (x, y) pairs.
    let subplots = subplots(&traces, &axes);
    for &(xi, yi) in &subplots {
        let (x0, x1) = span(&axes[xi]);
        let (y0, y1) = span(&axes[yi]);
        if ctx.theme.plot_bg.a > 0. {
            svg.fill_rect(x0, y0, x1 - x0, y1 - y0, ctx.theme.plot_bg);
        }
    }
    // Grid + zero lines under the data.
    for &(xi, yi) in &subplots {
        let has_fill_x = traces.iter().any(|t| t.xa == xi && t.has_bars_or_fill(true));
        let has_fill_y = traces.iter().any(|t| t.ya == yi && t.has_bars_or_fill(false));
        let ys = span(&axes[yi]);
        let xs = span(&axes[xi]);
        axes[xi].draw_grid(&mut svg, ys, has_fill_x);
        axes[yi].draw_grid(&mut svg, xs, has_fill_y);
        // Overlaying y axes draw their own grid in the same area.
        for (k, a) in axes.iter().enumerate() {
            if a.letter == 'y' && a.overlaying.as_deref() == Some(axes[yi].id.as_str()) && k != yi {
                let has = traces.iter().any(|t| t.ya == k && t.has_bars_or_fill(false));
                a.draw_grid(&mut svg, xs, has);
            }
        }
    }
    // Shapes below the data.
    draw_shapes(&ctx, &mut svg, &axes, &area, true, &mut notes);
    // Traces, in plotly's layer order, clipped to their subplot.
    let mut clip_ids: HashMap<(usize, usize), String> = HashMap::new();
    for layer in [Kind::Image, Kind::Heatmap, Kind::Bar, Kind::Box, Kind::Scatter] {
        for t in traces.iter() {
            let tk = match t.kind {
                Kind::Histogram => Kind::Bar,
                Kind::Histogram2d => Kind::Heatmap,
                Kind::Candlestick | Kind::Ohlc => Kind::Box,
                k => k,
            };
            if tk != layer || t.legend_only {
                continue;
            }
            let key = (t.xa, t.ya);
            let (x0, x1) = span(&axes[t.xa]);
            let (y0, y1) = span(&axes[t.ya]);
            let id = clip_ids
                .entry(key)
                .or_insert_with(|| svg.clip_rect(x0, y0, x1 - x0, y1 - y0))
                .clone();
            svg.group_clip(&id);
            traces::draw(&ctx, t, &axes[t.xa], &axes[t.ya], &mut svg, &mut notes);
            svg.group_close();
        }
    }
    for t in traces.iter() {
        if t.kind == Kind::Pie && !t.legend_only {
            pie::draw(&ctx, t, &area, &mut svg);
        }
    }
    draw_shapes(&ctx, &mut svg, &axes, &area, false, &mut notes);
    // Axes on top.
    for i in 0..axes.len() {
        if !axis_used(&axes, &subplots, i) {
            continue;
        }
        let a = &axes[i];
        let mirror = mirror_pos(&axes, i);
        let depth = a.draw(&mut svg, mirror, &ctx);
        a.draw_title(&mut svg, depth, (a.p0 + a.p1) / 2.);
    }
    draw_annotations(&ctx, &mut svg, &axes, &area, &mut notes);
    let mut cb_x = area.px(1.02);
    for cb in &colorbars {
        cb.draw(&ctx, &mut svg, cb_x, &area);
        cb_x += cb.width();
    }
    if let Some(lg) = &legend {
        lg.draw(&ctx, &mut svg, &area, cb_x - area.px(1.02), &axes);
    }
    draw_title(&ctx, &mut svg, (width, height), &area, m[2]);
    Some(Rendered {
        svg: svg.finish(),
        width: width as f32,
        height: height as f32,
        notes,
    })
}

pub fn push_note(notes: &mut Vec<String>, n: &str) {
    if !notes.iter().any(|x| x == n) {
        notes.push(n.to_string());
    }
}

fn is_axis_key(k: &str) -> bool {
    (k.starts_with("xaxis") || k.starts_with("yaxis")) && k[5..].chars().all(|c| c.is_ascii_digit()) && !k[5..].starts_with('1')
        || (k.starts_with("xaxis") || k.starts_with("yaxis")) && k.len() > 5 && k[5..].chars().all(|c| c.is_ascii_digit()) && k[5..].len() >= 2
}

/// "x2" -> "xaxis2".
fn axis_name(id: &str, letter: char) -> Option<String> {
    let mut chars = id.chars();
    if chars.next() != Some(letter) {
        return None;
    }
    let rest = chars.as_str();
    if rest.is_empty() || rest == "1" {
        return Some(format!("{letter}axis"));
    }
    if rest.chars().all(|c| c.is_ascii_digit()) && rest.len() < 4 {
        Some(format!("{letter}axis{rest}"))
    } else {
        None
    }
}

fn resolve_theme(ctx: &mut Ctx) {
    let dark = ctx.opts.dark;
    let font_size = ctx.lay_num("font.size").filter(|s| *s > 1. && *s < 100.).unwrap_or(12.);
    let font_color = ctx.lay_color("font.color").unwrap_or(Rgba::hex_rgb(0x444444));
    let paper = ctx.lay_color("paper_bgcolor").unwrap_or(Rgba::WHITE);
    let plot = ctx.lay_color("plot_bgcolor").unwrap_or(Rgba::WHITE);
    let colorway: Vec<Rgba> = ctx
        .lay("colorway")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|c| c.as_str().and_then(Rgba::parse)).collect())
        .filter(|v: &Vec<Rgba>| !v.is_empty())
        .unwrap_or_else(|| PLOTLY_JS_COLORWAY.iter().map(|&c| Rgba::hex_rgb(c)).collect());
    let fig_dark = paper.a > 0.5 && paper.luminance() < 0.2;
    // plotly's default grid: font color mixed into the plot background.
    let default_grid = Rgba::mix(plot, font_color, 0.1);
    let mut th = Theme {
        dark,
        blend: ctx.opts.blend && !dark,
        fig_dark,
        font_color,
        font_size,
        paper_bg: paper,
        plot_bg: plot,
        orig_bgs: [paper, plot],
        default_grid,
        axis_default: Rgba::hex_rgb(0x444444),
        colorway,
    };
    if dark {
        th.paper_bg = Rgba::TRANSPARENT;
        th.plot_bg = if plot == paper || (!fig_dark && plot.luminance() > 0.95) {
            Rgba::TRANSPARENT
        } else if fig_dark {
            plot
        } else {
            Rgba::WHITE.with_alpha(0.05)
        };
        th.font_color = th.text(font_color);
    } else if th.blend && !fig_dark {
        // On the page: keep a background only if the figure itself set one
        // (not its template) and it isn't white.
        let own = |p: &str| util::path(ctx.layout, p).and_then(|v| v.as_str()).and_then(Rgba::parse);
        let keep = |c: Option<Rgba>| c.filter(|c| c.a > 0. && c.luminance() < 0.97);
        th.paper_bg = keep(own("paper_bgcolor")).unwrap_or(Rgba::TRANSPARENT);
        th.plot_bg = keep(own("plot_bgcolor")).unwrap_or(Rgba::TRANSPARENT);
    }
    ctx.theme = th;
}

/// The page's ink (Herse's foreground), for blended grids and lines.
const INK: Rgba = Rgba { r: 0x1c, g: 0x1a, b: 0x17, a: 1. };

/// Pixel positions of every axis from its domain and anchor.
fn place_axes(axes: &mut [Axis], area: &Area) {
    for a in axes.iter_mut() {
        let (d0, d1) = a.domain;
        if a.is_x() {
            a.p0 = area.px(d0);
            a.p1 = area.px(d1);
        } else {
            a.p0 = area.py(d0);
            a.p1 = area.py(d1);
        }
    }
}

/// Cross-axis position of every axis line (after domains are final).
fn place_lines(axes: &mut [Axis], area: &Area) {
    let n = axes.len();
    for i in 0..n {
        // plotly's default anchor is the first counter axis (x <-> y).
        let anchor = axes[i].anchor.clone().or_else(|| Some(if axes[i].is_x() { "y" } else { "x" }.to_string()));
        let other = anchor.as_deref().filter(|a| *a != "free").and_then(|a| axes.iter().position(|x| x.id == a));
        let side = axes[i].side;
        let pos = match other {
            Some(o) => {
                let oa = &axes[o];
                let (lo, hi) = (oa.p0.min(oa.p1), oa.p0.max(oa.p1));
                match side {
                    Side::Bottom => hi,
                    Side::Top => lo,
                    Side::Left => lo,
                    Side::Right => hi,
                }
            }
            None => {
                if anchor.as_deref() == Some("free") {
                    let p = axes[i].position;
                    if axes[i].is_x() { area.py(p) } else { area.px(p) }
                } else {
                    // Default: the first counter axis.
                    match side {
                        Side::Bottom => area.b(),
                        Side::Top => area.t,
                        Side::Left => area.l,
                        Side::Right => area.r(),
                    }
                }
            }
        };
        axes[i].line_pos = pos;
    }
}

fn autorange_all(axes: &mut [Axis]) {
    // Matched axes autorange together.
    let n = axes.len();
    let root = |axes: &[Axis], mut i: usize| {
        for _ in 0..8 {
            match axes[i].matches.as_deref().and_then(|m| axes.iter().position(|a| a.id == m)) {
                Some(j) if j != i => i = j,
                _ => break,
            }
        }
        i
    };
    let roots: Vec<usize> = (0..n).map(|i| root(axes, i)).collect();
    for r in 0..n {
        if roots[r] != r {
            continue;
        }
        let members: Vec<usize> = (0..n).filter(|&i| roots[i] == r).collect();
        if members.len() == 1 {
            axes[r].autorange();
            continue;
        }
        let mut merged = axes[r].clone();
        for &mi in &members {
            if mi == r {
                continue;
            }
            for e in axes[mi].ext_min.clone() {
                merged.expand_ext(e, true);
            }
            for e in axes[mi].ext_max.clone() {
                merged.expand_ext(e, false);
            }
        }
        merged.autorange();
        let range = merged.range;
        for &mi in &members {
            axes[mi].range = range;
        }
    }
}

/// scaleanchor constraints (px.imshow: square cells).
fn constrain(axes: &mut [Axis]) {
    for i in 0..axes.len() {
        let Some(target) = axes[i].scaleanchor.clone() else { continue };
        let Some(j) = axes.iter().position(|a| a.id == target) else { continue };
        if i == j {
            continue;
        }
        let ratio = axes[i].scaleratio;
        let span_i = (axes[i].range.1 - axes[i].range.0).abs();
        let span_j = (axes[j].range.1 - axes[j].range.0).abs();
        if span_i <= 0. || span_j <= 0. {
            continue;
        }
        let ppu_i = axes[i].len() / span_i;
        let ppu_j = axes[j].len() / span_j;
        // want ppu_i == ratio * ppu_j
        if ppu_i > ratio * ppu_j {
            shrink_or_expand(&mut axes[i], span_i * ratio * ppu_j);
        } else {
            shrink_or_expand(&mut axes[j], span_j * ppu_i / ratio);
        }
    }
}

fn shrink_or_expand(a: &mut Axis, new_len: f64) {
    if a.constrain_domain {
        let mid = (a.p0 + a.p1) / 2.;
        let dir = (a.p1 - a.p0).signum();
        a.p0 = mid - dir * new_len / 2.;
        a.p1 = mid + dir * new_len / 2.;
    } else {
        // Expand the range around its center to fill the length.
        let len = a.len();
        let (r0, r1) = a.range;
        let mid = (r0 + r1) / 2.;
        let half = (r1 - r0) / 2. * len / new_len.max(1e-9);
        a.range = (mid - half, mid + half);
    }
}

/// Margins needed so edge axes' labels and titles fit (plotly automargin).
fn needed_margins(axes: &[Axis], area: &Area) -> [f64; 4] {
    let mut need = [0f64; 4];
    for a in axes {
        if !a.st.visible || !a.st.automargin {
            continue;
        }
        let depth = a.label_gap() + a.label_depth() + a.title_depth() + 4.;
        match a.side {
            Side::Left if (a.line_pos - area.l).abs() < 2. => need[0] = need[0].max(depth),
            Side::Right if (a.line_pos - area.r()).abs() < 2. => need[1] = need[1].max(depth),
            Side::Top if (a.line_pos - area.t).abs() < 2. => need[2] = need[2].max(depth),
            Side::Bottom if (a.line_pos - area.b()).abs() < 2. => need[3] = need[3].max(depth),
            _ => {}
        }
        // Long x labels near the left/right edges.
        if a.is_x() && a.label_angle == 0. && a.st.showticklabels
            && let Some(t) = a.ticks.first() {
                let w = axis::lines_width(&t.lines, a.st.tickfont_size) / 2.;
                let over = w - (a.l2p(t.v) - 0.);
                if over > 0. {
                    need[0] = need[0].max(area.l + over + 2.);
                }
            }
    }
    need
}

/// (x, y) axis pairs that form subplots.
fn subplots(traces: &[Trace], axes: &[Axis]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let base = |i: usize| -> usize {
        match axes[i].overlaying.as_deref().and_then(|o| axes.iter().position(|a| a.id == o)) {
            Some(b) => b,
            None => i,
        }
    };
    let add = |x: usize, y: usize, out: &mut Vec<(usize, usize)>| {
        let p = (base(x), base(y));
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for t in traces {
        if t.kind != Kind::Pie && t.xa < axes.len() && t.ya < axes.len() {
            add(t.xa, t.ya, &mut out);
        }
    }
    // plotly draws only subplots with traces; a figure without any data
    // still shows its (layout) axes.
    if traces.iter().any(|t| t.kind == Kind::Pie || t.xa < axes.len()) {
        return out;
    }
    for (i, a) in axes.iter().enumerate() {
        if let Some(anc) = a.anchor.as_deref()
            && let Some(j) = axes.iter().position(|b| b.id == anc)
            && axes[j].letter != a.letter
        {
            if a.is_x() {
                add(i, j, &mut out);
            } else {
                add(j, i, &mut out);
            }
        }
    }
    if out.is_empty() {
        let x = axes.iter().position(|a| a.is_x());
        let y = axes.iter().position(|a| !a.is_x());
        if let (Some(x), Some(y)) = (x, y) {
            out.push((x, y));
        }
    }
    out
}

fn axis_used(axes: &[Axis], subplots: &[(usize, usize)], i: usize) -> bool {
    subplots.iter().any(|&(x, y)| x == i || y == i) || (axes[i].overlaying.is_some() && axes[i].has_data)
}

/// Where a mirrored axis line goes (the opposite side of its subplot).
fn mirror_pos(axes: &[Axis], i: usize) -> Option<f64> {
    let a = &axes[i];
    let anchor = a.anchor.as_deref()?;
    let o = axes.iter().find(|x| x.id == anchor)?;
    let (lo, hi) = (o.p0.min(o.p1), o.p0.max(o.p1));
    Some(match a.side {
        Side::Bottom => lo,
        Side::Top => hi,
        Side::Left => hi,
        Side::Right => lo,
    })
}

/// Pixel span of an axis (low, high).
pub fn span(a: &Axis) -> (f64, f64) {
    (a.p0.min(a.p1), a.p0.max(a.p1))
}

fn draw_title(ctx: &Ctx, svg: &mut Svg, (width, height): (f64, f64), area: &Area, top_margin: f64) {
    let title = match ctx.layout.get("title") {
        Some(Value::String(s)) => s.as_str(),
        Some(v) => v.get("text").and_then(|t| t.as_str()).unwrap_or(""),
        None => "",
    };
    if title.trim().is_empty() {
        return;
    }
    let th = &ctx.theme;
    let size = ctx.lay_num("title.font.size").unwrap_or(th.font_size * 1.4).round();
    let color = th.text(ctx.lay_color("title.font.color").unwrap_or(th.font_color));
    let xref_paper = ctx.lay_str("title.xref") == Some("paper");
    let x = ctx.lay_num("title.x").unwrap_or(0.5);
    let px = if xref_paper { area.px(x) } else { x * width };
    let anchor = match ctx.lay_str("title.xanchor") {
        Some("left") => Anchor::Start,
        Some("right") => Anchor::End,
        Some("center") => Anchor::Middle,
        _ => {
            if x < 1. / 3. {
                Anchor::Start
            } else if x > 2. / 3. {
                Anchor::End
            } else {
                Anchor::Middle
            }
        }
    };
    let lines = text::rich(title);
    let n = lines.len() as f64;
    // title.y: container (figure) fraction from the bottom, or paper.
    let block = (n - 1.) * size * 1.3;
    let mut y = match ctx.lay_num("title.y") {
        Some(yf) => {
            let py = if ctx.lay_str("title.yref") == Some("paper") { area.py(yf) } else { (1. - yf) * height };
            match ctx.lay_str("title.yanchor").unwrap_or("auto") {
                "top" => py + size * 0.75,
                "bottom" => py - block - size * 0.25,
                "middle" => py + size * 0.35 - block / 2.,
                _ if yf > 0.5 => py + size * 0.75,
                _ if yf < 0.5 => py - block - size * 0.25,
                _ => py + size * 0.35 - block / 2.,
            }
        }
        None => top_margin / 2. - block / 2.,
    };
    let style = TextStyle::new(size, color).anchor(anchor);
    for line in &lines {
        svg.text_runs(px, y, line, &style);
        y += size * 1.3;
    }
}

fn ref_x(r: &str, axes: &[Axis], area: &Area, v: &Value, is_shape: bool) -> Option<f64> {
    pos_ref(r, 'x', axes, area, v, is_shape)
}

/// Resolves an `xref` / `yref` position to px.
fn pos_ref(r: &str, letter: char, axes: &[Axis], area: &Area, v: &Value, is_shape: bool) -> Option<f64> {
    if r == "paper" || r.is_empty() {
        let f = util::num_loose(v)?;
        return Some(if letter == 'x' { area.px(f) } else { area.py(f) });
    }
    let (id, domain) = match r.strip_suffix(" domain") {
        Some(id) => (id, true),
        None => (r, false),
    };
    let a = axes.iter().find(|a| a.id == id)?;
    if domain {
        let f = util::num_loose(v)?;
        let (lo, hi) = (a.p0.min(a.p1), a.p0.max(a.p1));
        return Some(if letter == 'x' { lo + f * (hi - lo) } else { hi - f * (hi - lo) });
    }
    let l = if a.ty == AxisType::Log && !is_shape {
        // Annotation positions on log axes are given in log units.
        util::num_loose(v)
    } else if a.ty == AxisType::Category {
        match v {
            Value::String(s) => a.cat_index.get(s).map(|&i| i as f64),
            _ => util::num(v),
        }
    } else {
        a.d2l_value(v)
    }?;
    Some(a.l2p(l))
}

fn draw_shapes(ctx: &Ctx, svg: &mut Svg, axes: &[Axis], area: &Area, below: bool, notes: &mut Vec<String>) {
    let Some(shapes) = ctx.layout.get("shapes").and_then(|v| v.as_array()) else { return };
    for s in shapes.iter().take(500) {
        let layer_below = s.get("layer").and_then(|v| v.as_str()) == Some("below");
        if layer_below != below || s.get("visible").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let ty = s.get("type").and_then(|v| v.as_str()).unwrap_or(if s.get("path").is_some() { "path" } else { "rect" });
        let xref = s.get("xref").and_then(|v| v.as_str()).unwrap_or("x");
        let yref = s.get("yref").and_then(|v| v.as_str()).unwrap_or("y");
        let g = |k: &str| s.get(k).unwrap_or(&NULL);
        let line_color = s
            .get("line")
            .and_then(|l| l.get("color"))
            .and_then(|v| v.as_str())
            .and_then(Rgba::parse)
            .or_else(|| ctx.lay_color("shapedefaults.line.color"))
            .unwrap_or(Rgba::hex_rgb(0x444444));
        let line_width = s.get("line").and_then(|l| l.get("width")).and_then(util::num).unwrap_or(2.);
        let dash = s.get("line").and_then(|l| l.get("dash")).and_then(|v| v.as_str()).and_then(|d| dash_array(d, line_width));
        let fill = s.get("fillcolor").and_then(|v| v.as_str()).and_then(Rgba::parse);
        let opacity = s.get("opacity").and_then(util::num).unwrap_or(1.);
        let mut style = Style {
            fill: if ty == "line" { None } else { fill },
            stroke: Some(ctx.theme.text(line_color)),
            stroke_width: line_width,
            dash,
            opacity,
            round_joins: false,
        };
        if line_width <= 0. {
            style.stroke = None;
        }
        if ty == "path" {
            push_note(notes, "path shapes omitted");
            continue;
        }
        let (Some(x0), Some(x1), Some(y0), Some(y1)) = (
            ref_x(xref, axes, area, g("x0"), true),
            ref_x(xref, axes, area, g("x1"), true),
            pos_ref(yref, 'y', axes, area, g("y0"), true),
            pos_ref(yref, 'y', axes, area, g("y1"), true),
        ) else {
            continue;
        };
        match ty {
            "line" => svg.line(x0, y0, x1, y1, &style),
            "circle" => {
                let (cx, cy) = ((x0 + x1) / 2., (y0 + y1) / 2.);
                let (rx, ry) = (((x1 - x0) / 2.).abs(), ((y1 - y0) / 2.).abs());
                let mut d = crate::svg::PathData::new();
                d.move_to(cx + rx, cy);
                d.d.push_str(&format!(
                    "A{} {} 0 1 1 {} {}A{} {} 0 1 1 {} {}Z",
                    crate::svg::fmt_num(rx),
                    crate::svg::fmt_num(ry),
                    crate::svg::fmt_num(cx - rx),
                    crate::svg::fmt_num(cy),
                    crate::svg::fmt_num(rx),
                    crate::svg::fmt_num(ry),
                    crate::svg::fmt_num(cx + rx),
                    crate::svg::fmt_num(cy)
                ));
                svg.path(&d.d, &style);
            }
            _ => svg.rect(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs(), &style),
        }
    }
}

fn draw_annotations(ctx: &Ctx, svg: &mut Svg, axes: &[Axis], area: &Area, notes: &mut Vec<String>) {
    let Some(anns) = ctx.layout.get("annotations").and_then(|v| v.as_array()) else { return };
    let th = &ctx.theme;
    for a in anns.iter().take(500) {
        if a.get("visible").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let text_s = match a.get("text") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => util::label_of(&Value::Number(n.clone())),
            _ => continue,
        };
        let xref = a.get("xref").and_then(|v| v.as_str()).unwrap_or("x");
        let yref = a.get("yref").and_then(|v| v.as_str()).unwrap_or("y");
        let (Some(x), Some(y)) = (
            pos_ref(xref, 'x', axes, area, a.get("x").unwrap_or(&Value::from(0.5)), false),
            pos_ref(yref, 'y', axes, area, a.get("y").unwrap_or(&Value::from(0.5)), false),
        ) else {
            continue;
        };
        let size = a
            .get("font")
            .and_then(|f| f.get("size"))
            .and_then(util::num)
            .or_else(|| ctx.lay_num("annotationdefaults.font.size"))
            .unwrap_or(th.font_size);
        let color = a
            .get("font")
            .and_then(|f| f.get("color"))
            .and_then(|v| v.as_str())
            .and_then(Rgba::parse)
            .unwrap_or(th.font_color);
        let color = th.text(color);
        let lines = text::rich(&text_s);
        let n = lines.len() as f64;
        let w = axis::lines_width(&lines, size);
        let h = n * size * 1.3;
        let angle = a.get("textangle").and_then(util::num).unwrap_or(0.);
        let showarrow = a.get("showarrow").and_then(|v| v.as_bool()).unwrap_or(true);
        let (mut cx, mut cy) = (x, y);
        if showarrow {
            let ax = a.get("ax").and_then(util::num).unwrap_or(-10.);
            let ay = a.get("ay").and_then(util::num).unwrap_or(-30.);
            let axref_px = a.get("axref").and_then(|v| v.as_str()).unwrap_or("pixel") == "pixel";
            let ayref_px = a.get("ayref").and_then(|v| v.as_str()).unwrap_or("pixel") == "pixel";
            if !(axref_px && ayref_px) {
                push_note(notes, "annotation arrows approximated");
            }
            cx = x + ax;
            cy = y + ay;
            let arrow_color = a.get("arrowcolor").and_then(|v| v.as_str()).and_then(Rgba::parse).unwrap_or(color);
            let arrow_w = a.get("arrowwidth").and_then(util::num).unwrap_or(1.);
            // From the text box edge to the point, with a small head.
            let (dx, dy) = (x - cx, y - cy);
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > 1. {
                let (ux, uy) = (dx / dist, dy / dist);
                let tx = (w / 2. + 3.) / ux.abs().max(1e-9);
                let ty = (h / 2. + 3.) / uy.abs().max(1e-9);
                let start = tx.min(ty).min(dist);
                let sx = cx + ux * start;
                let sy = cy + uy * start;
                let style = Style::stroke(th.text(arrow_color), arrow_w);
                svg.line(sx, sy, x - ux * 2., y - uy * 2., &style);
                let head = 6. + arrow_w;
                let (px, py) = (-uy, ux);
                let mut d = crate::svg::PathData::new();
                d.move_to(x, y);
                d.line_to(x - ux * head + px * head * 0.45, y - uy * head + py * head * 0.45);
                d.line_to(x - ux * head - px * head * 0.45, y - uy * head - py * head * 0.45);
                d.close();
                svg.path(&d.d, &Style::fill(th.text(arrow_color)));
            }
        }
        // Box anchor.
        let xanchor = a.get("xanchor").and_then(|v| v.as_str()).unwrap_or("auto");
        let yanchor = a.get("yanchor").and_then(|v| v.as_str()).unwrap_or("auto");
        let paper_x = xref == "paper" || xref.ends_with(" domain");
        let fx = if paper_x && !showarrow { a.get("x").and_then(util::num).unwrap_or(0.5) } else { 0.5 };
        let (bw, bh) = if (angle.abs() - 90.).abs() < 1. { (h, w) } else { (w, h) };
        let left = match xanchor {
            "left" => cx,
            "right" => cx - bw,
            "center" => cx - bw / 2.,
            _ => {
                if showarrow {
                    cx - bw / 2.
                } else if fx < 1. / 3. {
                    cx
                } else if fx > 2. / 3. {
                    cx - bw
                } else {
                    cx - bw / 2.
                }
            }
        };
        let top = match yanchor {
            "top" => cy,
            "bottom" => cy - bh,
            _ => cy - bh / 2.,
        };
        // plotly pads annotation boxes by borderpad (1) + borderwidth.
        let pad = 1. + a.get("borderwidth").and_then(util::num).unwrap_or(1.).max(0.);
        let (left, top) = (
            left + match xanchor {
                "left" => pad,
                "right" => -pad,
                _ => 0.,
            },
            top + match yanchor {
                "top" => pad,
                "bottom" => -pad,
                _ => 0.,
            },
        );
        if let Some(bg) = a.get("bgcolor").and_then(|v| v.as_str()).and_then(Rgba::parse)
            && bg.a > 0.
        {
            svg.fill_rect(left - pad, top - pad, bw + 2. * pad, bh + 2. * pad, if th.dark { bg.scale_alpha(0.6) } else { bg });
        }
        let align = a.get("align").and_then(|v| v.as_str()).unwrap_or("center");
        if (angle.abs() - 90.).abs() < 1. {
            // Vertical text (facet row labels).
            let cxm = left + bw / 2.;
            let cym = top + bh / 2.;
            let style = TextStyle::new(size, color).anchor(Anchor::Middle).rotate(angle);
            let mut off = -(n - 1.) * size * 1.3 / 2.;
            for line in &lines {
                // Rotated by +90: text "down" is -x.
                let dir = if angle > 0. { -1. } else { 1. };
                svg.text_runs(cxm + -(dir * (off + size * 0.35)), cym, line, &style);
                off += size * 1.3;
            }
        } else {
            let style = TextStyle::new(size, color);
            let mut yy = top + size * 0.95;
            for line in &lines {
                let lw = text::runs_width(line, size);
                let (xx, anchor) = match align {
                    "left" => (left, Anchor::Start),
                    "right" => (left + bw, Anchor::End),
                    _ => (left + bw / 2., Anchor::Middle),
                };
                let _ = lw;
                svg.text_runs(xx, yy, line, &style.clone().anchor(anchor).rotate(angle));
                yy += size * 1.3;
            }
        }
    }
}

impl Trace<'_> {
    /// Bars or fills reaching this axis (keeps a zero line at the plot edge).
    pub fn has_bars_or_fill(&self, x_axis: bool) -> bool {
        match self.kind {
            Kind::Bar | Kind::Histogram => self.horizontal == x_axis,
            Kind::Scatter => {
                let fill = self.str("fill").unwrap_or("none");
                (x_axis && fill == "tozerox") || (!x_axis && (fill == "tozeroy" || self.get("stackgroup").is_some()))
            }
            _ => false,
        }
    }
}

/// Datum view used for autotype.
pub fn data_of<'a>(arr: &'a Arr<'a>) -> Vec<Datum<'a>> {
    (0..arr.len()).map(|i| arr.get(i)).collect()
}

impl Axis {
    /// Adds an existing extreme (matched axes).
    pub fn expand_ext(&mut self, e: axis::Ext, is_min: bool) {
        let arr = if is_min { &mut self.ext_min } else { &mut self.ext_max };
        arr.push(e);
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity)]
mod tests {
    use std::cell::RefCell;

    use super::traces::Calc;
    use super::{Axis, Trace};
    use crate::RenderOptions;

    #[derive(Default, Clone)]
    pub struct Snap {
        /// (axis name, range, tick labels)
        pub axes: Vec<(String, (f64, f64), Vec<String>)>,
        /// Bars per trace: (p0, p1, s0, s1)
        pub bars: Vec<Vec<(f64, f64, f64, f64)>>,
        /// Box stats per trace: (pos, q1, median, q3, lower fence, upper fence)
        pub boxes: Vec<Vec<(f64, f64, f64, f64, f64, f64)>>,
    }

    thread_local! {
        static SNAP: RefCell<Snap> = RefCell::new(Snap::default());
    }

    pub fn record(axes: &[Axis], traces: &[Trace]) {
        let mut s = Snap::default();
        for a in axes {
            let labels = a
                .ticks
                .iter()
                .map(|t| t.lines.iter().map(|l| l.iter().map(|r| r.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n"))
                .collect();
            s.axes.push((a.name.clone(), a.range, labels));
        }
        for t in traces {
            match &t.calc {
                Calc::Bars(b) => s.bars.push((0..b.p0.len()).map(|i| (b.p0[i], b.p1[i], b.s0[i], b.s1[i])).collect()),
                Calc::Box(b) => s.boxes.push(b.boxes.iter().map(|x| (x.pos, x.q1, x.med, x.q3, x.lf, x.uf)).collect()),
                _ => {}
            }
        }
        SNAP.with(|x| *x.borrow_mut() = s);
    }

    fn sample(name: &str) -> serde_json::Value {
        let p = format!("{}/tests/data/plotly/{name}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn snap(name: &str) -> Snap {
        let r = super::render(&sample(name), &RenderOptions::default());
        assert!(r.is_some(), "{name} should render");
        SNAP.with(|x| x.borrow().clone())
    }

    fn axis<'a>(s: &'a Snap, name: &str) -> &'a (String, (f64, f64), Vec<String>) {
        s.axes.iter().find(|a| a.0 == name).unwrap_or_else(|| panic!("no {name}"))
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn bar_ranges_start_at_zero_with_headroom() {
        let s = snap("bar_simple");
        let y = axis(&s, "yaxis");
        assert_eq!(y.1.0, 0.);
        assert!(close(y.1.1, 22. / 0.95, 1e-6), "{:?}", y.1);
        assert_eq!(y.2, ["0", "5", "10", "15", "20"]);
        let x = axis(&s, "xaxis");
        assert!(close(x.1.0, -0.5, 1e-9) && close(x.1.1, 4.5, 1e-9), "{:?}", x.1);
        assert_eq!(x.2, ["a", "b", "c", "d", "e"]);
        // bar width 0.8 (bargap 0.2) centred on the category
        let b = s.bars[0][1];
        assert!(close(b.0, 0.6, 1e-9) && close(b.1, 1.4, 1e-9) && b.3 == 18.);
    }

    #[test]
    fn stacked_bars_accumulate() {
        let s = snap("bar_stacked");
        let y = axis(&s, "yaxis");
        assert!(close(y.1.1, 1778.4 / 0.95, 0.01), "{:?}", y.1);
        // Second trace starts where the first ended (same category).
        let top_female_sun = s.bars[0].iter().filter(|b| close(b.0 + b.1, 0., 1e-6)).map(|b| b.3).fold(0., f64::max);
        let bottom_male_sun = s.bars[1].iter().filter(|b| close(b.0 + b.1, 0., 1e-6)).map(|b| b.2).fold(f64::INFINITY, f64::min);
        assert!(close(top_female_sun, bottom_male_sun, 1e-6));
        assert_eq!(axis(&s, "yaxis").2.last().map(String::as_str), Some("1500"));
    }

    #[test]
    fn scatter_markers_pad_and_lines_are_tight() {
        let s = snap("scatter_markers");
        let x = axis(&s, "xaxis");
        // data [-2.5168, 2.0004]; marker pad 3.75px + 5% of the axis length
        assert!(x.1.0 < -2.5168 && x.1.1 > 2.0004);
        let s = snap("scatter_lines");
        let x = axis(&s, "xaxis");
        assert_eq!(x.1, (0., 10.));
        let y = axis(&s, "yaxis");
        assert!(y.1.0 < -1. && y.1.1 > 1.);
    }

    #[test]
    fn histogram_bins_like_plotly() {
        let s = snap("hist_raw");
        let bars = &s.bars[0];
        let total: f64 = bars.iter().map(|b| b.3 - b.2).sum();
        assert_eq!(total, 244.);
        // size-2 bins starting at 2 (data 3.07..50.81)
        assert!(close(bars[0].0, 2., 1e-9) && close(bars[0].1, 4., 1e-9), "{:?}", bars[0]);
        let x = axis(&s, "xaxis");
        assert!(close(x.1.0, 2., 1e-9) && close(x.1.1, 52., 1e-9), "{:?}", x.1);
        // explicit xbins: trailing empty bins are dropped
        let s = snap("hist_xbins");
        let x = axis(&s, "xaxis");
        assert!(close(x.1.1, 55., 1e-9), "{:?}", x.1);
    }

    #[test]
    fn box_quartiles_use_plotlys_linear_method() {
        let s = snap("box");
        let sun = s.boxes[0].iter().find(|b| b.0 == 0.).unwrap();
        assert!(close(sun.1, 14.935, 1e-3) && close(sun.2, 19.63, 1e-3) && close(sun.3, 25.635, 1e-3), "{sun:?}");
        let y = axis(&s, "yaxis");
        // whiskers/outliers 3.07..50.81 plus 5% padding each side
        assert!(close(y.1.0, 0.418, 0.01) && close(y.1.1, 53.46, 0.01), "{:?}", y.1);
    }

    #[test]
    fn date_log_and_si_labels() {
        let s = snap("line_dates");
        assert_eq!(axis(&s, "xaxis").2[..2], ["Jan 2018".to_string(), "Jul 2018".to_string()]);
        let s = snap("line_dates_hourly");
        assert_eq!(axis(&s, "xaxis").2[0], "00:00\nMar 1, 2024");
        assert_eq!(axis(&s, "xaxis").2[1], "12:00");
        let s = snap("log_y");
        assert_eq!(axis(&s, "yaxis").2, ["1", "100", "10k", "1M"]);
        let s = snap("dark_template");
        assert!(axis(&s, "yaxis").2.contains(&"30k".to_string()));
        let s = snap("tickformat_reversed");
        let y = axis(&s, "yaxis");
        assert!(y.1.0 > y.1.1, "reversed");
        assert!(y.2.contains(&"40%".to_string()));
        assert!(axis(&s, "xaxis").2.contains(&"2 s".to_string()));
    }

    #[test]
    fn facets_share_matched_ranges_and_bins() {
        let s = snap("facet_grid");
        let r1 = axis(&s, "xaxis").1;
        let r3 = axis(&s, "xaxis3").1;
        assert_eq!(r1, r3);
        assert_eq!(axis(&s, "yaxis").1, axis(&s, "yaxis8").1);
        // bingroup spans the facets: every bar has the same width
        let w: Vec<f64> = s.bars.iter().flat_map(|t| t.iter().map(|b| ((b.1 - b.0) * 1e6).round())).collect();
        assert!(w.windows(2).all(|p| p[0] == p[1]));
    }
}
