//! `pie [showData] [title …]` with `"label" : value` lines: slices clockwise
//! from 12 o'clock, largest first (as d3's pie orders them), percentages on
//! the slices and a legend on the right.

use super::{Doc, Theme, label_lines, text_line};
use crate::svg::{self, Anchor, PathData, Style};
use crate::text;

const R: f64 = 120.;
const SWATCH: f64 = 12.;
const ROW: f64 = 24.;

pub fn render(lines: &[String], th: &Theme) -> Option<Doc> {
    let mut header = lines.first()?.trim().strip_prefix("pie")?.trim();
    let mut show_data = false;
    let mut title: Option<String> = None;
    if let Some(r) = header.strip_prefix("showData") {
        show_data = true;
        header = r.trim();
    }
    if let Some(t) = header.strip_prefix("title") {
        title = Some(t.trim().to_string());
    } else if !header.is_empty() {
        return None;
    }
    let mut slices: Vec<(String, f64)> = Vec::new();
    for l in &lines[1..] {
        let l = l.trim();
        if l.is_empty() || l.starts_with("accTitle") || l.starts_with("accDescr") {
            continue;
        }
        if l == "showData" {
            show_data = true;
            continue;
        }
        if let Some(t) = l.strip_prefix("title") {
            title = Some(t.trim().to_string());
            continue;
        }
        let (label, value) = parse_slice(l)?;
        if let Some(s) = slices.iter_mut().find(|s| s.0 == label) {
            // mermaid keeps the last value for a repeated label.
            s.1 = value;
        } else {
            slices.push((label, value));
        }
    }
    if slices.is_empty() || slices.len() > 200 {
        return None;
    }
    let total: f64 = slices.iter().map(|s| s.1).sum();
    if !(total > 0. && total.is_finite()) {
        return None;
    }
    // Largest first; ties keep their order.
    let mut order: Vec<usize> = (0..slices.len()).collect();
    order.sort_by(|&a, &b| slices[b].1.total_cmp(&slices[a].1));
    let color = |k: usize| {
        let c = th.series[k % th.series.len()];
        // Past eight slices, lighter steps of the same hues.
        let round = (k / th.series.len()) as f64;
        crate::color::Rgba::mix(c, th.bg, (0.35 * round).min(0.7))
    };

    let mut out = String::new();
    let (cx, cy) = (R + 2., R + 2.);
    let shown: Vec<usize> = order.iter().copied().filter(|&i| slices[i].1 > 0.).collect();
    let mut a0 = 0f64;
    let sep = Style { stroke: Some(th.bg), stroke_width: 2., opacity: 1., round_joins: true, ..Default::default() };
    for (k, &i) in shown.iter().enumerate() {
        let frac = slices[i].1 / total;
        let a1 = a0 + frac * std::f64::consts::TAU;
        let fill = color(k);
        let st = Style { fill: Some(fill), ..sep.clone() };
        if shown.len() == 1 {
            out.push_str("<circle cx=\"");
            svg::num(&mut out, cx);
            out.push_str("\" cy=\"");
            svg::num(&mut out, cy);
            out.push_str("\" r=\"");
            svg::num(&mut out, R);
            out.push('"');
            Style::fill(fill).write(&mut out);
            out.push_str("/>\n");
        } else {
            let pt = |a: f64| (cx + R * a.sin(), cy - R * a.cos());
            let (p0, p1) = (pt(a0), pt(a1));
            let mut d = PathData::new();
            d.move_to(cx, cy);
            d.line_to(p0.0, p0.1);
            d.arc_to(R, a1 - a0 > std::f64::consts::PI, true, p1.0, p1.1);
            d.close();
            out.push_str("<path d=\"");
            out.push_str(&d.d);
            out.push('"');
            st.write(&mut out);
            out.push_str("/>\n");
        }
        a0 = a1;
    }
    // Percentages, where they fit.
    let mut a0 = 0f64;
    let size = th.small;
    for (k, &i) in shown.iter().enumerate() {
        let frac = slices[i].1 / total;
        let a1 = a0 + frac * std::f64::consts::TAU;
        let mid = (a0 + a1) / 2.;
        let pct = format!("{}%", (frac * 100. + 0.5).floor());
        let tw = text::width(&pct, size, true);
        let r = R * 0.72;
        let arc = (a1 - a0) * r;
        if shown.len() == 1 || (arc > tw + 6. && a1 - a0 > 0.28) {
            let fill = color(k);
            let ink = if fill.luminance() > 0.4 { th_dark_ink(th) } else { crate::color::Rgba::WHITE };
            text_line(&mut out, cx + r * mid.sin(), cy - r * mid.cos(), &pct, size, ink, 600, Anchor::Middle);
        }
        a0 = a1;
    }
    // Legend.
    let font = th.font;
    let lx = 2. * R + 4. + 32.;
    let n = order.len();
    let legend_h = n as f64 * ROW;
    let ly0 = cy - legend_h / 2.;
    let mut legend_w: f64 = 0.;
    for (k, &i) in order.iter().enumerate() {
        let (label, value) = &slices[i];
        let y = ly0 + ROW * (k as f64 + 0.5);
        out.push_str("<rect x=\"");
        svg::num(&mut out, lx);
        out.push_str("\" y=\"");
        svg::num(&mut out, y - SWATCH / 2.);
        out.push_str("\" width=\"12\" height=\"12\" rx=\"3\"");
        Style::fill(color(k)).write(&mut out);
        out.push_str("/>\n");
        let label_text = label_lines(label, font).join(" ");
        let tx = lx + SWATCH + 8.;
        text_line(&mut out, tx, y, &label_text, font, th.text, 400, Anchor::Start);
        let mut w = text::width(&label_text, font, false);
        if show_data {
            let v = format!("[{}]", number(*value));
            let vx = tx + w + 6.;
            text_line(&mut out, vx, y, &v, font, th.muted, 400, Anchor::Start);
            w += 6. + text::width(&v, font, false);
        }
        legend_w = legend_w.max(SWATCH + 8. + w);
    }
    let w = lx + legend_w + 2.;
    let h = 2. * R + 4.;
    let (body, h) = if legend_h > h {
        // A tall legend: centre the pie on it instead.
        let dy = (legend_h - h) / 2.;
        (format!("<g transform=\"translate(0 {})\">\n{}</g>\n", svg::fmt_num(dy), out), legend_h)
    } else {
        (out, h)
    };
    Some(Doc { body, w, h, title: title.filter(|t| !t.is_empty()), notes: Vec::new() })
}

fn th_dark_ink(th: &Theme) -> crate::color::Rgba {
    if th.dark { th.bg } else { th.text }
}

/// `"label" : value`
fn parse_slice(l: &str) -> Option<(String, f64)> {
    let q = l.chars().next()?;
    if q != '"' && q != '\'' {
        return None;
    }
    let end = l[1..].find(q)? + 1;
    let label = l[1..end].to_string();
    let rest = l[end + 1..].trim().strip_prefix(':')?.trim();
    let v: f64 = rest.parse().ok()?;
    if !(v.is_finite() && v >= 0.) {
        return None;
    }
    Some((label, v))
}

fn number(v: f64) -> String {
    if v.fract() == 0. && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices() {
        assert_eq!(parse_slice("\"Dogs\" : 386"), Some(("Dogs".into(), 386.)));
        assert_eq!(parse_slice("'Cats':85.5"), Some(("Cats".into(), 85.5)));
        assert_eq!(parse_slice("\"Rats\" : -1"), None);
        assert_eq!(parse_slice("Rats : 1"), None);
        assert_eq!(number(42.), "42");
        assert_eq!(number(2.50), "2.5");
    }

    #[test]
    fn headers() {
        let th = Theme::new(false);
        let doc = |src: &str| render(&src.lines().map(String::from).collect::<Vec<_>>(), &th);
        assert_eq!(doc("pie title Pets\n\"Dogs\" : 3\n\"Cats\" : 1").unwrap().title.as_deref(), Some("Pets"));
        assert!(doc("pie showData\ntitle Key\n\"A\" : 1").unwrap().body.contains("[1]"));
        assert!(doc("pie\n\"A\" : 0").is_none());
        assert!(doc("pie bogus\n\"A\" : 1").is_none());
        assert!(doc("pie\nA : 1").is_none());
    }
}
