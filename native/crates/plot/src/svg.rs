//! A small SVG writer: compact numbers, escaped text, a few element helpers.

use std::fmt::Write as _;

use crate::color::Rgba;

/// Appends `v` with at most `decimals` fractional digits, no trailing zeros.
pub fn num_p(out: &mut String, v: f64, decimals: u32) {
    if !v.is_finite() {
        out.push('0');
        return;
    }
    let scale = 10f64.powi(decimals as i32);
    let r = (v * scale).round();
    if r == 0. {
        out.push('0');
        return;
    }
    if r.abs() >= 9.0e15 {
        let _ = write!(out, "{}", v.round());
        return;
    }
    if r < 0. {
        out.push('-');
    }
    let r = r.abs() as u64;
    let s = scale as u64;
    let int = r / s;
    let mut frac = r % s;
    let _ = write!(out, "{int}");
    if frac != 0 {
        out.push('.');
        let mut digits = decimals;
        while frac.is_multiple_of(10) {
            frac /= 10;
            digits -= 1;
        }
        let _ = write!(out, "{frac:0width$}", width = digits as usize);
    }
}

pub fn num(out: &mut String, v: f64) {
    num_p(out, v, 2)
}

pub fn fmt_num(v: f64) -> String {
    let mut s = String::new();
    num(&mut s, v);
    s
}

/// XML-escapes text content / attribute values.
pub fn esc(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => out.push(' '),
            '\u{FFFE}' | '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
}

/// Writes ` name="#rrggbb"` plus ` name-opacity` when translucent, or
/// ` name="none"` for a fully transparent color.
pub fn paint(out: &mut String, name: &str, c: Option<Rgba>) {
    match c {
        Some(c) if c.a > 0.002 => {
            let _ = write!(out, " {name}=\"{}\"", c.hex());
            if c.a < 0.998 {
                let _ = write!(out, " {name}-opacity=\"");
                num_p(out, c.a as f64, 3);
                out.push('"');
            }
        }
        _ => {
            let _ = write!(out, " {name}=\"none\"");
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Style {
    pub fill: Option<Rgba>,
    pub stroke: Option<Rgba>,
    pub stroke_width: f64,
    /// SVG dash array, e.g. "6,3".
    pub dash: Option<String>,
    /// Group opacity (1 = opaque).
    pub opacity: f64,
    pub round_joins: bool,
}

impl Style {
    pub fn fill(c: Rgba) -> Style {
        Style {
            fill: Some(c),
            opacity: 1.,
            ..Default::default()
        }
    }

    pub fn stroke(c: Rgba, width: f64) -> Style {
        Style {
            stroke: Some(c),
            stroke_width: width,
            opacity: 1.,
            ..Default::default()
        }
    }

    pub fn write(&self, out: &mut String) {
        paint(out, "fill", self.fill);
        if let Some(s) = self.stroke.filter(|s| s.a > 0.002 && self.stroke_width > 0.)
        {
            paint(out, "stroke", Some(s));
            if (self.stroke_width - 1.).abs() > 1e-9 {
                out.push_str(" stroke-width=\"");
                num(out, self.stroke_width);
                out.push('"');
            }
            if let Some(d) = &self.dash {
                out.push_str(" stroke-dasharray=\"");
                esc(out, d);
                out.push('"');
            }
            if self.round_joins {
                out.push_str(" stroke-linejoin=\"round\" stroke-linecap=\"round\"");
            }
        }
        if self.opacity < 0.998 {
            out.push_str(" opacity=\"");
            num_p(out, self.opacity.max(0.), 3);
            out.push('"');
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

#[derive(Clone, Debug)]
pub struct TextStyle {
    pub size: f64,
    pub color: Rgba,
    pub anchor: Anchor,
    pub bold: bool,
    pub italic: bool,
    /// Degrees, clockwise, around the text's (x, y).
    pub rotate: f64,
    pub opacity: f64,
}

impl TextStyle {
    pub fn new(size: f64, color: Rgba) -> TextStyle {
        TextStyle {
            size,
            color,
            anchor: Anchor::Start,
            bold: false,
            italic: false,
            rotate: 0.,
            opacity: 1.,
        }
    }
    pub fn anchor(mut self, a: Anchor) -> Self {
        self.anchor = a;
        self
    }
    pub fn bold(mut self, b: bool) -> Self {
        self.bold = b;
        self
    }
    pub fn rotate(mut self, deg: f64) -> Self {
        self.rotate = deg;
        self
    }
}

/// One styled piece of a text line (see `text::rich`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    /// -1 subscript, 0 normal, 1 superscript.
    pub shift: i8,
    pub color: Option<Rgba>,
}

pub struct Svg {
    pub out: String,
    clip_ids: usize,
}

impl Svg {
    pub fn new(w: f64, h: f64, font_family: &str, base_size: f64) -> Svg {
        let mut out = String::with_capacity(16 * 1024);
        out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
        num(&mut out, w);
        out.push_str("\" height=\"");
        num(&mut out, h);
        out.push_str("\" viewBox=\"0 0 ");
        num(&mut out, w);
        out.push(' ');
        num(&mut out, h);
        out.push_str("\" font-family=\"");
        esc(&mut out, font_family);
        out.push_str("\" font-size=\"");
        num(&mut out, base_size);
        out.push_str("\">\n");
        Svg { out, clip_ids: 0 }
    }

    /// A `w`×`h` px document showing the `vw`×`vh` user space, scaled to fit.
    pub fn with_view(w: f64, h: f64, vw: f64, vh: f64, font_family: &str, base_size: f64) -> Svg {
        let mut s = Svg::new(vw, vh, font_family, base_size);
        let head_end = s.out.find(" viewBox").unwrap_or(0);
        let mut head = String::with_capacity(96);
        head.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
        num(&mut head, w);
        head.push_str("\" height=\"");
        num(&mut head, h);
        head.push('"');
        s.out.replace_range(..head_end, &head);
        s
    }

    /// Elements only, no `<svg>` wrapper (`finish` is not for these).
    pub fn fragment() -> Svg {
        Svg { out: String::with_capacity(8 * 1024), clip_ids: 0 }
    }

    pub fn finish(mut self) -> String {
        self.out.push_str("</svg>\n");
        self.out
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, style: &Style) {
        let (x, w) = if w < 0. { (x + w, -w) } else { (x, w) };
        let (y, h) = if h < 0. { (y + h, -h) } else { (y, h) };
        let o = &mut self.out;
        o.push_str("<rect x=\"");
        num(o, x);
        o.push_str("\" y=\"");
        num(o, y);
        o.push_str("\" width=\"");
        num(o, w);
        o.push_str("\" height=\"");
        num(o, h);
        o.push('"');
        style.write(o);
        o.push_str("/>\n");
    }

    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: Rgba) {
        self.rect(x, y, w, h, &Style::fill(c));
    }

    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, style: &Style) {
        let o = &mut self.out;
        o.push_str("<path d=\"M");
        num(o, x1);
        o.push(' ');
        num(o, y1);
        o.push('L');
        num(o, x2);
        o.push(' ');
        num(o, y2);
        o.push('"');
        let mut s = style.clone();
        s.fill = None;
        s.write(o);
        o.push_str("/>\n");
    }

    pub fn path(&mut self, d: &str, style: &Style) {
        if d.is_empty() {
            return;
        }
        let o = &mut self.out;
        o.push_str("<path d=\"");
        o.push_str(d);
        o.push('"');
        style.write(o);
        o.push_str("/>\n");
    }

    pub fn circle(&mut self, cx: f64, cy: f64, r: f64, style: &Style) {
        let o = &mut self.out;
        o.push_str("<circle cx=\"");
        num(o, cx);
        o.push_str("\" cy=\"");
        num(o, cy);
        o.push_str("\" r=\"");
        num(o, r);
        o.push('"');
        style.write(o);
        o.push_str("/>\n");
    }

    /// Plain text with its baseline at `y`.
    pub fn text(&mut self, x: f64, y: f64, s: &str, t: &TextStyle) {
        if s.is_empty() {
            return;
        }
        self.text_open(x, y, t);
        esc(&mut self.out, s);
        self.out.push_str("</text>\n");
    }

    /// Styled runs on one line, baseline at `y`.
    pub fn text_runs(&mut self, x: f64, y: f64, runs: &[Run], t: &TextStyle) {
        if runs.iter().all(|r| r.text.is_empty()) {
            return;
        }
        if runs.len() == 1 && runs[0].shift == 0 && !runs[0].bold && !runs[0].italic && runs[0].color.is_none() {
            return self.text(x, y, &runs[0].text, t);
        }
        self.text_open(x, y, t);
        let mut shift = 0.;
        for r in runs {
            let o = &mut self.out;
            o.push_str("<tspan");
            if r.bold && !t.bold {
                o.push_str(" font-weight=\"bold\"");
            }
            if r.italic && !t.italic {
                o.push_str(" font-style=\"italic\"");
            }
            if let Some(c) = r.color {
                paint(o, "fill", Some(c));
            }
            let want = match r.shift {
                1 => -0.4 * t.size,
                -1 => 0.25 * t.size,
                _ => 0.,
            };
            if (want - shift) != 0. {
                o.push_str(" dy=\"");
                num(o, want - shift);
                o.push('"');
                shift = want;
            }
            if r.shift != 0 {
                o.push_str(" font-size=\"");
                num(o, t.size * 0.7);
                o.push('"');
            }
            o.push('>');
            esc(o, &r.text);
            o.push_str("</tspan>");
        }
        self.out.push_str("</text>\n");
    }

    fn text_open(&mut self, x: f64, y: f64, t: &TextStyle) {
        let o = &mut self.out;
        o.push_str("<text x=\"");
        num(o, x);
        o.push_str("\" y=\"");
        num(o, y);
        o.push('"');
        if (t.size - 12.).abs() > 1e-9 {
            o.push_str(" font-size=\"");
            num(o, t.size);
            o.push('"');
        }
        match t.anchor {
            Anchor::Start => {}
            Anchor::Middle => o.push_str(" text-anchor=\"middle\""),
            Anchor::End => o.push_str(" text-anchor=\"end\""),
        }
        if t.bold {
            o.push_str(" font-weight=\"bold\"");
        }
        if t.italic {
            o.push_str(" font-style=\"italic\"");
        }
        paint(o, "fill", Some(t.color));
        if t.opacity < 0.998 {
            o.push_str(" opacity=\"");
            num_p(o, t.opacity, 3);
            o.push('"');
        }
        if t.rotate != 0. {
            o.push_str(" transform=\"rotate(");
            num(o, t.rotate);
            o.push(' ');
            num(o, x);
            o.push(' ');
            num(o, y);
            o.push_str(")\"");
        }
        o.push('>');
    }

    /// Defines a rectangular clip path and returns its id.
    pub fn clip_rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> String {
        self.clip_ids += 1;
        let id = format!("c{}", self.clip_ids);
        let o = &mut self.out;
        let _ = write!(o, "<clipPath id=\"{id}\"><rect x=\"");
        num(o, x);
        o.push_str("\" y=\"");
        num(o, y);
        o.push_str("\" width=\"");
        num(o, w.max(0.));
        o.push_str("\" height=\"");
        num(o, h.max(0.));
        o.push_str("\"/></clipPath>\n");
        id
    }

    pub fn group_clip(&mut self, clip: &str) {
        let _ = writeln!(self.out, "<g clip-path=\"url(#{clip})\">");
    }

    pub fn group_opacity(&mut self, opacity: f64) {
        self.out.push_str("<g opacity=\"");
        num_p(&mut self.out, opacity.clamp(0., 1.), 3);
        self.out.push_str("\">\n");
    }

    pub fn group_close(&mut self) {
        self.out.push_str("</g>\n");
    }
}

/// Builds path data with compact numbers.
#[derive(Default)]
pub struct PathData {
    pub d: String,
    pub decimals: u32,
}

impl PathData {
    pub fn new() -> PathData {
        PathData {
            d: String::new(),
            decimals: 2,
        }
    }
    pub fn with_decimals(decimals: u32) -> PathData {
        PathData {
            d: String::new(),
            decimals,
        }
    }
    fn pair(&mut self, x: f64, y: f64) {
        num_p(&mut self.d, x, self.decimals);
        self.d.push(' ');
        num_p(&mut self.d, y, self.decimals);
    }
    pub fn move_to(&mut self, x: f64, y: f64) {
        self.d.push('M');
        self.pair(x, y);
    }
    pub fn line_to(&mut self, x: f64, y: f64) {
        self.d.push('L');
        self.pair(x, y);
    }
    pub fn cubic_to(&mut self, c1: (f64, f64), c2: (f64, f64), p: (f64, f64)) {
        self.d.push('C');
        self.pair(c1.0, c1.1);
        self.d.push(' ');
        self.pair(c2.0, c2.1);
        self.d.push(' ');
        self.pair(p.0, p.1);
    }
    /// Arc to (x, y) with radius r.
    pub fn arc_to(&mut self, r: f64, large: bool, sweep: bool, x: f64, y: f64) {
        self.d.push('A');
        self.pair(r, r);
        self.d.push_str(if large { " 0 1 " } else { " 0 0 " });
        self.d.push_str(if sweep { "1 " } else { "0 " });
        self.pair(x, y);
    }
    pub fn close(&mut self) {
        self.d.push('Z');
    }
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.move_to(x, y);
        self.d.push('h');
        num_p(&mut self.d, w, self.decimals);
        self.d.push('v');
        num_p(&mut self.d, h, self.decimals);
        self.d.push('h');
        num_p(&mut self.d, -w, self.decimals);
        self.d.push('Z');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_numbers() {
        let f = |v: f64| fmt_num(v);
        assert_eq!(f(0.), "0");
        assert_eq!(f(1.), "1");
        assert_eq!(f(1.5), "1.5");
        assert_eq!(f(-1.25), "-1.25");
        assert_eq!(f(0.004), "0");
        assert_eq!(f(-0.004), "0");
        assert_eq!(f(12.345), "12.35");
        assert_eq!(f(3.10), "3.1");
        assert_eq!(f(0.05), "0.05");
        assert_eq!(f(f64::NAN), "0");
        assert_eq!(f(1e20), "100000000000000000000");
    }

    #[test]
    fn escapes() {
        let mut s = String::new();
        esc(&mut s, "a<b & \"c\"\u{1}");
        assert_eq!(s, "a&lt;b &amp; &quot;c&quot; ");
    }
}
