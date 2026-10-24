//! plotly legends.

use super::traces::{Calc, Each, Fill, Symbol};
use super::{Area, Axis, Ctx, Kind, Trace};
use crate::color::Rgba;
use crate::svg::{PathData, Run, Style, Svg, TextStyle};
use crate::text;

#[derive(Clone, Debug)]
pub enum Swatch {
    Line {
        color: Rgba,
        width: f64,
        dash: Option<String>,
        marker: Option<(Symbol, Rgba, f64, f64)>,
        fill: Option<Rgba>,
    },
    Marker(Symbol, Rgba, f64, f64, Option<(Rgba, f64)>),
    Rect { fill: Rgba, line: Option<(Rgba, f64)>, opacity: f64 },
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: Vec<Vec<Run>>,
    pub swatch: Swatch,
    pub group: String,
    pub dim: bool,
}

pub struct Legend {
    pub items: Vec<Item>,
    pub title: Vec<Vec<Run>>,
    pub horizontal: bool,
    pub x: f64,
    pub y: f64,
    pub xanchor: String,
    pub yanchor: String,
    pub width: f64,
    pub height: f64,
    pub font_size: f64,
    pub font_color: Rgba,
    pub title_size: f64,
    pub bg: Rgba,
    pub border: Option<(Rgba, f64)>,
    pub group_gap: f64,
    /// Horizontal layout: rows of (item index, x offset).
    pub rows: Vec<Vec<(usize, f64)>>,
}

const ITEM_TEXT_X: f64 = 40.;

fn item_height(it: &Item, fs: f64) -> f64 {
    let lines = it.label.len().max(1) as f64;
    (lines * fs * 1.3).max(16.) + 3.
}

pub fn build(ctx: &Ctx, traces: &[Trace]) -> Option<Legend> {
    let th = &ctx.theme;
    let mut items: Vec<Item> = Vec::new();
    let mut legend_traces = 0;
    let mut has_pie = false;
    let mut reversed_default = false;
    let horizontal = ctx.lay_str("legend.orientation") == Some("h");
    for t in traces {
        if !t.showlegend {
            continue;
        }
        if t.kind == Kind::Pie {
            has_pie = true;
            if let Calc::Pie(p) = &t.calc {
                for s in &p.slices {
                    items.push(Item {
                        label: text::rich(&s.label),
                        swatch: Swatch::Rect {
                            fill: s.color,
                            line: p.line.and_then(|(c, w)| th.outline(c).map(|c| (c, w))),
                            opacity: 1.,
                        },
                        group: String::new(),
                        dim: false,
                    });
                }
            }
            continue;
        }
        if t.name.is_empty() && t.raw.get("name").is_some() && t.raw.get("showlegend").is_none() {
            // plotly still lists unnamed traces; px hides them with showlegend=false.
        }
        legend_traces += 1;
        let swatch = match &t.calc {
            Calc::Scatter(s) => {
                // plotly reverses legends of explicit tonext fills (but not stackgroups).
                if s.stackgroup.is_none() && matches!(t.str("fill"), Some("tonexty" | "tonextx")) {
                    reversed_default = true;
                }
                let marker = if s.markers {
                    let r = match &s.marker.radius {
                        Each::One(r) => r.min(8.),
                        Each::Many(_) => 6.,
                    };
                    let r = if ctx.lay_str("legend.itemsizing") == Some("constant") && s.marker.array_size { 6. } else { r };
                    Some((s.marker.symbol.first(), s.marker.color.first(), r, s.marker.opacity.first().clamp(0., 1.)))
                } else {
                    None
                };
                let fill = (s.fill != Fill::None).then_some(s.fillcolor);
                if s.lines || fill.is_some() {
                    Swatch::Line {
                        color: s.line_color,
                        width: if s.lines { s.line_width.min(5.) } else { 0. },
                        dash: s.dash.clone(),
                        marker,
                        fill,
                    }
                } else if let Some((sym, c, r, op)) = marker {
                    let line = (s.marker.line_width > 0.).then(|| th.outline(s.marker.line_color.first()).map(|c| (c, s.marker.line_width.min(2.)))).flatten();
                    Swatch::Marker(sym, c, r, op, line)
                } else {
                    continue;
                }
            }
            Calc::Bars(b) => {
                if ctx.lay_str("barmode") == Some("stack") {
                    reversed_default = true;
                }
                Swatch::Rect {
                    fill: b.color.first(),
                    line: (b.line_width > 0.).then(|| th.outline(b.line_color.first()).map(|c| (c, b.line_width.min(2.)))).flatten(),
                    opacity: b.opacity,
                }
            }
            Calc::Box(b) => Swatch::Rect {
                fill: b.fillcolor,
                line: Some((b.line_color, b.line_width.min(2.))),
                opacity: 1.,
            },
            _ => continue,
        };
        items.push(Item {
            label: text::rich(&t.name),
            swatch,
            group: t.legendgroup.clone(),
            dim: t.legend_only,
        });
    }
    let show_default = legend_traces > 1 || has_pie;
    let show = ctx.layout.get("showlegend").and_then(|v| v.as_bool()).unwrap_or(show_default);
    if !show || items.is_empty() {
        return None;
    }
    let order = ctx.lay_str("legend.traceorder").map(str::to_string);
    let grouped = items.iter().any(|i| !i.group.is_empty());
    if grouped && order.as_deref().is_none_or(|o| o.contains("grouped")) {
        // Stable grouping by first appearance.
        let mut groups: Vec<String> = Vec::new();
        for it in &items {
            if !groups.contains(&it.group) {
                groups.push(it.group.clone());
            }
        }
        let mut sorted = Vec::with_capacity(items.len());
        for g in &groups {
            sorted.extend(items.iter().filter(|i| &i.group == g).cloned());
        }
        items = sorted;
    }
    let reversed = match order.as_deref() {
        Some(o) => o.contains("reversed"),
        None => reversed_default && !horizontal,
    };
    if reversed {
        items.reverse();
    }
    let font_size = ctx.lay_num("legend.font.size").unwrap_or(th.font_size);
    let font_color = th.text(ctx.lay_color("legend.font.color").unwrap_or(th.font_color));
    let title = match ctx.lay("legend.title") {
        Some(serde_json::Value::String(s)) => text::rich(s),
        Some(v) => v.get("text").and_then(|t| t.as_str()).map(text::rich).unwrap_or_default(),
        None => Vec::new(),
    };
    let title_size = ctx.lay_num("legend.title.font.size").unwrap_or((th.font_size * 1.2).round());
    let bg = ctx.lay_color("legend.bgcolor").unwrap_or(th.paper_bg);
    let bg = if (th.dark || th.blend) && !th.fig_dark { Rgba::TRANSPARENT } else { bg };
    let bw = ctx.lay_num("legend.borderwidth").unwrap_or(0.);
    let border = (bw > 0.).then(|| (th.line(ctx.lay_color("legend.bordercolor").unwrap_or(Rgba::hex_rgb(0x444444))), bw));
    let group_gap = if grouped { ctx.lay_num("legend.tracegroupgap").unwrap_or(10.) } else { 0. };
    let (x, xanchor) = (
        ctx.lay_num("legend.x").unwrap_or(if horizontal { 0. } else { 1.02 }),
        ctx.lay_str("legend.xanchor").unwrap_or("auto").to_string(),
    );
    let (y, yanchor) = (
        ctx.lay_num("legend.y").unwrap_or(if horizontal { -0.1 } else { 1. }),
        ctx.lay_str("legend.yanchor").unwrap_or(if horizontal { "top" } else { "auto" }).to_string(),
    );
    let mut lg = Legend {
        items,
        title,
        horizontal,
        x,
        y,
        xanchor,
        yanchor,
        width: 0.,
        height: 0.,
        font_size,
        font_color,
        title_size,
        bg,
        border,
        group_gap,
        rows: Vec::new(),
    };
    lg.measure(ctx.opts.width as f64 * 0.9);
    Some(lg)
}

impl Legend {
    fn title_w(&self) -> f64 {
        crate::plotly::axis::lines_width(&self.title, self.title_size)
    }
    fn title_h(&self) -> f64 {
        if self.title.is_empty() { 0. } else { self.title.len() as f64 * self.title_size * 1.3 + 5. }
    }

    fn measure(&mut self, max_w: f64) {
        let fs = self.font_size;
        if !self.horizontal {
            let tw = self.items.iter().map(|i| crate::plotly::axis::lines_width(&i.label, fs)).fold(0., f64::max);
            self.width = (ITEM_TEXT_X + tw + 6.).max(self.title_w() + 6.);
            let mut h = 5. + self.title_h();
            let mut prev_group: Option<&str> = None;
            for it in &self.items {
                if let Some(g) = prev_group
                    && g != it.group
                {
                    h += self.group_gap;
                }
                prev_group = Some(&it.group);
                h += item_height(it, fs);
            }
            self.height = h + 5.;
        } else {
            let title_w = if self.title.is_empty() { 0. } else { self.title_w() + 8. };
            let mut rows: Vec<Vec<(usize, f64)>> = vec![Vec::new()];
            let mut x = title_w;
            let mut widest: f64 = 0.;
            for (i, it) in self.items.iter().enumerate() {
                let w = ITEM_TEXT_X + crate::plotly::axis::lines_width(&it.label, fs) + 10.;
                if x + w > max_w && !rows.last().is_none_or(|r| r.is_empty()) {
                    rows.push(Vec::new());
                    x = title_w;
                }
                if let Some(r) = rows.last_mut() {
                    r.push((i, x));
                }
                x += w;
                widest = widest.max(x);
            }
            let row_h = self.items.iter().map(|i| item_height(i, fs)).fold(19., f64::max);
            self.width = widest + 4.;
            self.height = rows.len() as f64 * row_h + 10.;
            self.rows = rows;
        }
    }

    /// Vertical legend outside the plot on the right?
    pub fn outside_right(&self) -> bool {
        !self.horizontal && self.x >= 1.
    }

    /// Margins (top, bottom) a horizontal legend outside the plot needs.
    pub fn vertical_push(&self, ph: f64) -> (f64, f64) {
        if !self.horizontal {
            return (0., 0.);
        }
        let ya = self.resolved_yanchor();
        if self.y > 1. || (self.y >= 1. && ya == "bottom") {
            let top = match ya {
                "bottom" => (self.y - 1.) * ph + self.height,
                "middle" => (self.y - 1.) * ph + self.height / 2.,
                _ => (self.y - 1.) * ph,
            };
            (top + 10., 0.)
        } else if self.y < 0. {
            // Below the x axis labels and title.
            (0., 80. + self.height)
        } else {
            (0., 0.)
        }
    }

    fn resolved_yanchor(&self) -> &str {
        match self.yanchor.as_str() {
            "top" | "bottom" | "middle" => &self.yanchor,
            _ => {
                if self.y >= 2. / 3. {
                    "top"
                } else if self.y <= 1. / 3. {
                    "bottom"
                } else {
                    "middle"
                }
            }
        }
    }

    pub fn draw(&self, ctx: &Ctx, svg: &mut Svg, area: &Area, right_shift: f64, axes: &[Axis]) {
        let th = &ctx.theme;
        let xa = match self.xanchor.as_str() {
            "left" | "right" | "center" => self.xanchor.as_str(),
            _ => {
                if self.x >= 1. || self.x <= 1. / 3. {
                    "left"
                } else if self.x >= 2. / 3. {
                    "right"
                } else {
                    "center"
                }
            }
        };
        let px = area.px(self.x) + if self.x >= 1. { right_shift } else { 0. };
        let left = match xa {
            "right" => px - self.width,
            "center" => px - self.width / 2.,
            _ => px,
        };
        let mut py = area.py(self.y);
        if self.horizontal && self.y < 0. {
            // Keep clear of the bottom axis labels / title.
            let below = axes
                .iter()
                .filter(|a| a.is_x() && (a.line_pos - area.b()).abs() < 2.)
                .map(|a| a.line_pos + a.label_gap() + a.label_depth() + a.title_depth() + 6.)
                .fold(py, f64::max);
            py = below;
        }
        let top = match self.resolved_yanchor() {
            "bottom" => py - self.height,
            "middle" => py - self.height / 2.,
            _ => py,
        };
        if self.bg.a > 0. || self.border.is_some() {
            let mut st = Style::fill(self.bg);
            if let Some((c, w)) = self.border {
                st.stroke = Some(c);
                st.stroke_width = w;
            }
            svg.rect(left, top, self.width, self.height, &st);
        }
        let fs = self.font_size;
        let text_style = TextStyle::new(fs, self.font_color);
        if !self.horizontal {
            let mut y = top + 5.;
            if !self.title.is_empty() {
                let ts = TextStyle::new(self.title_size, self.font_color);
                let mut ty = top + self.title_size * 1.3;
                for l in &self.title {
                    svg.text_runs(left + 2., ty, l, &ts);
                    ty += self.title_size * 1.3;
                }
                y += self.title_h();
            }
            let mut prev_group: Option<&str> = None;
            for it in &self.items {
                if let Some(g) = prev_group
                    && g != it.group
                {
                    y += self.group_gap;
                }
                prev_group = Some(&it.group);
                let h = item_height(it, fs);
                let cy = y + h / 2.;
                self.draw_item(svg, it, left, cy, &text_style);
                y += h;
            }
        } else {
            let row_h = self.items.iter().map(|i| item_height(i, fs)).fold(19., f64::max);
            if !self.title.is_empty() {
                let ts = TextStyle::new(self.title_size, self.font_color);
                svg.text_runs(left + 2., top + 5. + row_h / 2. + self.title_size * 0.35, &self.title[0], &ts);
            }
            for (r, row) in self.rows.iter().enumerate() {
                let cy = top + 5. + r as f64 * row_h + row_h / 2.;
                for &(i, x) in row {
                    self.draw_item(svg, &self.items[i], left + x, cy, &text_style);
                }
            }
        }
        let _ = th;
    }

    fn draw_item(&self, svg: &mut Svg, it: &Item, x: f64, cy: f64, ts: &TextStyle) {
        let fs = self.font_size;
        if it.dim {
            svg.group_opacity(0.5);
        }
        let sx = x + 20.;
        match &it.swatch {
            Swatch::Line { color, width, dash, marker, fill } => {
                if let Some(f) = fill {
                    let mut d = PathData::new();
                    d.rect(x + 5., cy, 30., 6.);
                    svg.path(&d.d, &Style::fill(*f));
                }
                if *width > 0. {
                    let mut st = Style::stroke(*color, *width);
                    st.dash = dash.clone();
                    svg.line(x + 5., cy, x + 35., cy, &st);
                }
                if let Some((sym, c, r, op)) = marker {
                    marker_swatch(svg, *sym, *c, *r, *op, None, sx, cy);
                }
            }
            Swatch::Marker(sym, c, r, op, line) => marker_swatch(svg, *sym, *c, *r, *op, *line, sx, cy),
            Swatch::Rect { fill, line, opacity } => {
                let mut st = Style::fill(*fill);
                if let Some((lc, lw)) = line {
                    st.stroke = Some(*lc);
                    st.stroke_width = *lw;
                }
                st.opacity = *opacity;
                svg.rect(sx - 6., cy - 6., 12., 12., &st);
            }
        }
        let n = it.label.len() as f64;
        let mut ty = cy + fs * 0.39 - (n - 1.) * fs * 1.3 / 2.;
        for l in &it.label {
            svg.text_runs(x + ITEM_TEXT_X, ty, l, ts);
            ty += fs * 1.3;
        }
        if it.dim {
            svg.group_close();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn marker_swatch(svg: &mut Svg, sym: Symbol, c: Rgba, r: f64, op: f64, line: Option<(Rgba, f64)>, x: f64, y: f64) {
    let mut d = PathData::new();
    sym.path(&mut d, x, y, r);
    let mut st = if sym.open { Style::stroke(c, 1.) } else { Style::fill(c) };
    if let (Some((lc, lw)), false) = (line, sym.open) {
        st.stroke = Some(lc);
        st.stroke_width = lw;
    }
    st.opacity = op;
    svg.path(&d.d, &st);
}
