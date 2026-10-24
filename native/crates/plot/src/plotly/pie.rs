//! plotly pie / donut charts.

use std::collections::HashMap;

use serde_json::Value;

use super::{Area, Ctx, Trace};
use crate::color::Rgba;
use crate::format;
use crate::svg::{Anchor, PathData, Style, Svg, TextStyle};
use crate::text;
use crate::util::{self, Arr};

pub struct Slice {
    pub label: String,
    pub value: f64,
    pub color: Rgba,
    pub text: Option<String>,
}

pub struct PieCalc {
    pub slices: Vec<Slice>,
    pub hole: f64,
    pub clockwise: bool,
    pub rotation: f64,
    pub textinfo: String,
    pub textposition: String,
    pub line: Option<(Rgba, f64)>,
    pub domain: ((f64, f64), (f64, f64)),
    pub title: String,
    pub text_size: f64,
    pub texttemplate: Option<String>,
}

pub fn calc(ctx: &Ctx, t: &Trace) -> Option<PieCalc> {
    let labels = t.arr("labels");
    let values = t.arr("values");
    let n = match (&labels, &values) {
        (Some(l), Some(v)) => l.len().min(v.len()).max(if l.is_empty() { v.len() } else { 0 }),
        (Some(l), None) => l.len(),
        (None, Some(v)) => v.len(),
        _ => 0,
    };
    let label0 = t.num("label0").unwrap_or(0.);
    let dlabel = t.num("dlabel").unwrap_or(1.);
    let label_at = |i: usize| -> String {
        match &labels {
            Some(l) if i < l.len() => l.get(i).label(),
            _ => format::js_string(label0 + i as f64 * dlabel),
        }
    };
    let texts: Option<Arr> = t.arr("text");
    let colors_given: Option<Arr> = t.arr("marker.colors");
    // Aggregate by label, in first-appearance order.
    let mut order: Vec<String> = Vec::new();
    let mut sums: HashMap<String, (f64, Option<Rgba>, Option<String>)> = HashMap::new();
    for i in 0..n {
        let label = label_at(i);
        let v = match &values {
            Some(v) => v.num(i).unwrap_or(f64::NAN),
            None => 1.,
        };
        if !(v.is_finite() && v >= 0.) {
            continue;
        }
        let color = colors_given.as_ref().and_then(|c| match c.get(i) {
            util::Datum::Str(s) => Rgba::parse(s),
            _ => None,
        });
        let txt = texts.as_ref().map(|a| a.get(i).label());
        let e = sums.entry(label.clone()).or_insert_with(|| {
            order.push(label.clone());
            (0., color, txt)
        });
        e.0 += v;
    }
    let way: Vec<Rgba> = ctx
        .lay("piecolorway")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|c| c.as_str().and_then(Rgba::parse)).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| ctx.theme.colorway.clone());
    let mut slices: Vec<(Slice, Option<Rgba>)> = order
        .iter()
        .map(|l| {
            let (v, c, txt) = sums.remove(l).unwrap_or((0., None, None));
            (
                Slice {
                    label: l.clone(),
                    value: v,
                    color: Rgba::BLACK,
                    text: txt,
                },
                c,
            )
        })
        .collect();
    if t.get("sort").and_then(|v| v.as_bool()).unwrap_or(true) {
        slices.sort_by(|a, b| b.0.value.total_cmp(&a.0.value));
    }
    // Default colors go to slices in (sorted) display order.
    let mut k = 0;
    let slices: Vec<Slice> = slices
        .into_iter()
        .map(|(mut s, c)| {
            s.color = c.unwrap_or_else(|| {
                k += 1;
                way[(k - 1) % way.len()]
            });
            s
        })
        .collect();
    let dom = |k: &str| -> (f64, f64) {
        t.get(&format!("domain.{k}"))
            .and_then(|v| v.as_array())
            .and_then(|a| Some((util::num(a.first()?)?, util::num(a.get(1)?)?)))
            .unwrap_or((0., 1.))
    };
    let line_w = t.num("marker.line.width").unwrap_or(0.);
    let line_c = t.color_attr("marker.line.color").unwrap_or(Rgba::hex_rgb(0x444444));
    let title = match t.get("title") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        None => String::new(),
    };
    Some(PieCalc {
        slices,
        hole: t.num("hole").unwrap_or(0.).clamp(0., 0.99),
        clockwise: t.str("direction") == Some("clockwise"),
        rotation: t.num("rotation").unwrap_or(0.),
        textinfo: t.str("textinfo").unwrap_or("percent").to_string(),
        textposition: t.str("textposition").unwrap_or("auto").to_string(),
        line: (line_w > 0.).then_some((line_c, line_w)),
        domain: (dom("x"), dom("y")),
        title,
        text_size: t.num("textfont.size").unwrap_or(ctx.theme.font_size),
        texttemplate: t.str("texttemplate").map(str::to_string),
    })
}

/// plotly's `transformInsideText` (auto orientation): the best of a
/// horizontal box inscribed in the slice's circle, a tangential and a radial
/// rotation. Returns (center radius, rotation degrees, scale); plotly moves
/// `auto` text outside when the scale would be < 1.
fn inside_fit(r: f64, hole: f64, span: f64, mid: f64, tw: f64, th: f64) -> (f64, f64, f64) {
    let ring = 1. - hole;
    let half = (span / 2.).min(std::f64::consts::PI);
    let full = span >= std::f64::consts::TAU - 1e-9;
    let normalize = |mut deg: f64| {
        while deg > 90. {
            deg -= 180.;
        }
        while deg <= -90. {
            deg += 180.;
        }
        deg
    };
    let r_ins = if full && hole == 0. {
        1.
    } else {
        (1. / (1. + 1. / half.min(std::f64::consts::FRAC_PI_2).sin().max(1e-9))).min(ring / 2.)
    };
    // Horizontal first (plotly keeps text upright whenever it fits): the
    // most central radius where the text box lies inside the wedge.
    let lo = mid - span / 2.;
    let r_in = r * hole;
    let inside = |x: f64, y: f64| -> bool {
        let d = (x * x + y * y).sqrt();
        if d > r - 1. || d < r_in + 1. {
            return false;
        }
        if full {
            return true;
        }
        let a = x.atan2(-y);
        (a - lo).rem_euclid(std::f64::consts::TAU) <= span
    };
    if full && hole == 0. {
        return (0., 0., 1.);
    }
    let mut k = if hole > 0. { 0.5 } else { 0.55 };
    while k <= 0.95 {
        let rr = r_in + (r - r_in) * k;
        let (x, y) = (rr * mid.sin(), -rr * mid.cos());
        let (hw, hh) = (tw / 2. + 2., th / 2. + 1.);
        if [(x - hw, y - hh), (x + hw, y - hh), (x - hw, y + hh), (x + hw, y + hh)].iter().all(|&(px, py)| inside(px, py)) {
            return (rr, 0., 1.);
        }
        k += 0.05;
    }
    let mut best = (r * (1. - r_ins), 0., r_ins * r * 2. / (tw * tw + th * th).sqrt().max(1e-9));
    if !full {
        let re = (r - 6.).max(0.);
        let max_half = |a: f64| {
            let q = a + 1. / (2. * half.tan().max(1e-9));
            re * (1. / ((q * q + 0.5).sqrt() + q)).min(ring / ((a * a + ring / 2.).sqrt() + a))
        };
        // tangential: text runs across the radius
        let a = th / tw.max(1e-9);
        let sz = max_half(a);
        let t = ((re * re - sz * sz).max(0.).sqrt() - a * sz, normalize(mid.to_degrees()), 2. * sz / tw.max(1e-9));
        if t.2 > best.2 {
            best = t;
        }
        // radial: text runs along the radius
        let a = tw / th.max(1e-9);
        let sz = max_half(a);
        let rad = ((re * re - sz * sz).max(0.).sqrt() - a * sz, normalize(mid.to_degrees() - 90.), 2. * sz / th.max(1e-9));
        if rad.2 > best.2 {
            best = rad;
        }
    }
    best
}

fn percent(v: f64) -> String {
    let mut s = format::js_precision(v * 100., 3);
    if s.contains('.') && !s.contains('e') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    format!("{}%", format::num_separate(&s, false))
}

pub fn draw(ctx: &Ctx, t: &Trace, area: &Area, svg: &mut Svg) {
    let super::traces::Calc::Pie(p) = &t.calc else { return };
    let total: f64 = p.slices.iter().map(|s| s.value).sum();
    if total.is_nan() || total <= 0. {
        return;
    }
    let ((dx0, dx1), (dy0, dy1)) = p.domain;
    let x0 = area.px(dx0);
    let x1 = area.px(dx1);
    let y0 = area.py(dy1);
    let y1 = area.py(dy0);
    let has_title = !p.title.is_empty();
    let title_h = if has_title { ctx.theme.font_size * 1.6 } else { 0. };
    let cx = (x0 + x1) / 2.;
    let cy = (y0 + title_h + y1) / 2.;
    let r = ((x1 - x0).min(y1 - y0 - title_h) / 2.).max(5.);
    let r_in = r * p.hole;
    // plotly: angles clockwise from 12 o'clock.
    let mut angle = p.rotation.to_radians();
    let factor = std::f64::consts::TAU / total;
    let dir = if p.clockwise { 1. } else { -1. };
    if !p.clockwise {
        angle += factor * p.slices[0].value;
    }
    let at = |a: f64, rr: f64| (cx + rr * a.sin(), cy - rr * a.cos());
    let mut labels = Vec::new();
    for s in &p.slices {
        let a0 = angle;
        let a1 = angle + dir * factor * s.value;
        angle = a1;
        if s.value <= 0. {
            continue;
        }
        let mut d = PathData::new();
        let full = s.value >= total * 0.99999;
        if full {
            let (ax, ay) = at(0., r);
            d.move_to(ax, ay);
            let (bx, by) = at(std::f64::consts::PI, r);
            d.arc_to(r, true, true, bx, by);
            d.arc_to(r, true, true, ax, ay);
            d.close();
            if r_in > 0. {
                let (ix, iy) = at(0., r_in);
                d.move_to(ix, iy);
                let (jx, jy) = at(std::f64::consts::PI, r_in);
                d.arc_to(r_in, true, false, jx, jy);
                d.arc_to(r_in, true, false, ix, iy);
                d.close();
            }
        } else {
            let large = (a1 - a0).abs() > std::f64::consts::PI;
            let sweep = dir > 0.;
            let (sx, sy) = at(a0, r);
            let (ex, ey) = at(a1, r);
            if r_in > 0. {
                let (isx, isy) = at(a0, r_in);
                let (iex, iey) = at(a1, r_in);
                d.move_to(isx, isy);
                d.line_to(sx, sy);
                d.arc_to(r, large, sweep, ex, ey);
                d.line_to(iex, iey);
                d.arc_to(r_in, large, !sweep, isx, isy);
            } else {
                d.move_to(cx, cy);
                d.line_to(sx, sy);
                d.arc_to(r, large, sweep, ex, ey);
            }
            d.close();
        }
        let mut st = Style::fill(s.color);
        if let Some((c, w)) = p.line
            && let Some(c) = ctx.theme.outline(c)
        {
            st.stroke = Some(c);
            st.stroke_width = w;
        }
        svg.path(&d.d, &st);
        let mid = (a0 + a1) / 2.;
        labels.push((s, mid, (a1 - a0).abs()));
    }
    if p.textposition == "none" || p.textinfo == "none" {
        return;
    }
    let fs = p.text_size;
    for (s, mid, span) in labels {
        let frac = s.value / total;
        let txt = match &p.texttemplate {
            Some(tt) => super::traces::template(tt, |name, fmt| match name {
                "label" => Some(s.label.clone()),
                "value" => Some(match fmt {
                    Some(f) => format::d3(f, s.value),
                    None => format::plotly_hover_num(s.value, &format::ExpFormat::default()),
                }),
                "percent" => Some(match fmt {
                    Some(f) => format::d3(f, frac),
                    None => percent(frac),
                }),
                "text" => s.text.clone(),
                _ => None,
            }),
            None => {
                let mut parts = Vec::new();
                for part in p.textinfo.split('+') {
                    match part {
                        "label" => parts.push(s.label.clone()),
                        "value" => parts.push(format::plotly_hover_num(s.value, &format::ExpFormat::default())),
                        "percent" => parts.push(percent(frac)),
                        "text" => {
                            if let Some(t) = &s.text {
                                parts.push(t.clone());
                            }
                        }
                        _ => {}
                    }
                }
                parts.join("<br>")
            }
        };
        if txt.is_empty() {
            continue;
        }
        let lines = text::rich(&txt);
        let tw = super::axis::lines_width(&lines, fs);
        let th = lines.len() as f64 * fs * 1.2;
        let (rr, rot, scale) = inside_fit(r, p.hole, span, mid, tw, th);
        let inside = match p.textposition.as_str() {
            "inside" => Some((rr, rot, scale.min(1.))),
            "auto" if scale >= 1. => Some((rr, rot, 1.)),
            _ => None,
        };
        if let Some((rr, rot, scale)) = inside {
            let (x, y) = (cx + rr * mid.sin(), cy - rr * mid.cos());
            let fs = fs * scale;
            let style = TextStyle::new(fs, s.color.contrast_text()).anchor(Anchor::Middle).rotate(rot);
            let n = lines.len() as f64;
            let th_rad = rot.to_radians();
            // Lines stack along the rotated "down" direction.
            let (dx, dy) = (-th_rad.sin(), th_rad.cos());
            for (k, l) in lines.iter().enumerate() {
                let off = (k as f64 - (n - 1.) / 2.) * fs * 1.2 + fs * 0.35;
                svg.text_runs(x + dx * off, y + dy * off, l, &style);
            }
        } else {
            let ro = r + 6.;
            let (x, y) = (cx + ro * mid.sin(), cy - ro * mid.cos());
            let anchor = if mid.sin() > 0.1 {
                Anchor::Start
            } else if mid.sin() < -0.1 {
                Anchor::End
            } else {
                Anchor::Middle
            };
            let style = TextStyle::new(fs, ctx.theme.font_color).anchor(anchor);
            let mut ty = y + fs * 0.35 * (-mid.cos()).max(-0.5) + if mid.cos() < 0. { fs * 0.5 } else { 0. };
            for l in &lines {
                svg.text_runs(x, ty, l, &style);
                ty += fs * 1.2;
            }
        }
    }
    if has_title {
        let style = TextStyle::new(ctx.theme.font_size, ctx.theme.font_color).anchor(Anchor::Middle);
        svg.text_runs(cx, y0 + ctx.theme.font_size, &text::rich(&p.title).concat(), &style);
    }
}

#[cfg(test)]
mod tests {
    use super::percent;

    #[test]
    fn pie_percent_format() {
        assert_eq!(percent(0.456), "45.6%");
        assert_eq!(percent(1.), "100%");
        assert_eq!(percent(0.0912345), "9.12%");
        assert_eq!(percent(0.25), "25%");
    }
}

#[cfg(test)]
mod fit_tests {
    #[test]
    fn donut_slice_fits_horizontally() {
        let (rr, rot, scale) = super::inside_fit(110., 0.4, 1.837, -0.918, 34., 14.4);
        assert_eq!((rot, scale), (0., 1.), "rr {rr}");
    }
}
