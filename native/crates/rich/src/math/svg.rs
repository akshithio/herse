//! Box tree -> compact SVG. Glyph outlines are `<path>`s in `<defs>` (one per
//! glyph id, shared by every use), coordinates are milli-em (1 font unit of
//! STIX Two Math), and everything is filled with one color (`currentColor`
//! unless asked otherwise) so GPUI can tint the alpha mask.

use std::fmt::Write as _;
use std::sync::OnceLock;

use skrifa::outline::OutlinePen;

use super::tex::{BoxContent, Dim, MIDDLE_PLACEHOLDER, MathBox, MathFont};
use super::{Fill, MathError, MathSvg};

/// A glyph outline in font units, y pointing down, plus its ink box.
struct Glyph {
    d: Box<str>,
    /// x_min, y_min, x_max, y_max (y down).
    bbox: [f32; 4],
}

fn glyph_table(font: &MathFont) -> &'static [OnceLock<Option<Glyph>>] {
    static T: OnceLock<Box<[OnceLock<Option<Glyph>>]>> = OnceLock::new();
    T.get_or_init(|| {
        (0..=u16::MAX as usize)
            .take(font.glyph_count() as usize)
            .map(|_| OnceLock::new())
            .collect()
    })
}

fn glyph(font: &MathFont, gid: u16) -> Option<&'static Glyph> {
    let slot = glyph_table(font).get(gid as usize)?;
    slot.get_or_init(|| {
        let mut pen = Pen::default();
        if !font.draw_glyph(gid, &mut pen) || pen.d.is_empty() {
            return None;
        }
        Some(Glyph {
            d: pen.d.into_boxed_str(),
            bbox: [pen.min_x, pen.min_y, pen.max_x, pen.max_y],
        })
    })
    .as_ref()
}

struct Pen {
    d: String,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Default for Pen {
    fn default() -> Self {
        Pen {
            d: String::new(),
            min_x: f32::MAX,
            min_y: f32::MAX,
            max_x: f32::MIN,
            max_y: f32::MIN,
        }
    }
}

impl Pen {
    fn pt(&mut self, x: f32, y: f32) {
        let y = -y;
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
        num(&mut self.d, x as f64);
        self.d.push(' ');
        num(&mut self.d, y as f64);
    }
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.d.push('M');
        self.pt(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.d.push('L');
        self.pt(x, y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.d.push('Q');
        self.pt(cx0, cy0);
        self.d.push(' ');
        self.pt(x, y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.d.push('C');
        self.pt(cx0, cy0);
        self.d.push(' ');
        self.pt(cx1, cy1);
        self.d.push(' ');
        self.pt(x, y);
    }
    fn close(&mut self) {
        self.d.push('Z');
    }
}

/// Write `v` with at most one decimal, no trailing zeros, no `-0`.
fn num(out: &mut String, v: f64) {
    let r = (v * 10.0).round() / 10.0;
    if r == 0.0 {
        out.push('0');
    } else if r.fract() == 0.0 {
        let _ = write!(out, "{}", r as i64);
    } else {
        let _ = write!(out, "{r:.1}");
    }
}

fn f(d: &Dim) -> Result<f64, MathError> {
    match d.as_ratio() {
        Some((n, den)) if den != 0 => Ok(n as f64 / den as f64),
        _ => Err(MathError::Internal("non-finite dimension".into())),
    }
}

const MEM: f64 = 1000.0; // milli-em per em

struct Writer<'a> {
    font: &'a MathFont,
    fu_to_mem: f64,
    colors: bool,
    defs: String,
    body: String,
    used: Vec<u64>, // bitset over glyph ids
    /// Ink + box extents in milli-em (y down, baseline at 0).
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Writer<'_> {
    fn extend(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        self.min_x = self.min_x.min(x0.min(x1));
        self.max_x = self.max_x.max(x0.max(x1));
        self.min_y = self.min_y.min(y0.min(y1));
        self.max_y = self.max_y.max(y0.max(y1));
    }

    fn glyph(&mut self, gid: u16, x: f64, y: f64, scale: f64) -> Result<(), MathError> {
        let Some(g) = glyph(self.font, gid) else {
            return Ok(()); // blank glyph (space)
        };
        let i = gid as usize;
        if self.used[i / 64] & (1 << (i % 64)) == 0 {
            self.used[i / 64] |= 1 << (i % 64);
            let _ = write!(self.defs, r#"<path id="g{gid}" d="{}"/>"#, g.d);
        }
        let s = scale * self.fu_to_mem;
        let [bx0, by0, bx1, by1] = g.bbox;
        self.extend(
            x + bx0 as f64 * s,
            y + by0 as f64 * s,
            x + bx1 as f64 * s,
            y + by1 as f64 * s,
        );
        self.body.push_str(r##"<use xlink:href="#g"##);
        let _ = write!(self.body, "{gid}\"");
        if (s - 1.0).abs() < 1e-6 {
            self.body.push_str(" x=\"");
            num(&mut self.body, x);
            self.body.push_str("\" y=\"");
            num(&mut self.body, y);
            self.body.push_str("\"/>");
        } else {
            self.body.push_str(" transform=\"translate(");
            num(&mut self.body, x);
            self.body.push(' ');
            num(&mut self.body, y);
            let _ = write!(self.body, ") scale({:.4})\"/>", s);
            // Trim the scale's trailing zeros for compactness.
            trim_scale(&mut self.body);
        }
        Ok(())
    }

    fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, extra: &str) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        self.extend(x, y, x + w, y + h);
        self.body.push_str("<rect x=\"");
        num(&mut self.body, x);
        self.body.push_str("\" y=\"");
        num(&mut self.body, y);
        self.body.push_str("\" width=\"");
        num(&mut self.body, w.max(0.1));
        self.body.push_str("\" height=\"");
        num(&mut self.body, h.max(0.1));
        self.body.push('"');
        self.body.push_str(extra);
        self.body.push_str("/>");
    }
}

fn trim_scale(s: &mut String) {
    // "scale(0.7000)\"/>" -> "scale(0.7)\"/>"
    let tail = ")\"/>";
    if let Some(stripped) = s.strip_suffix(tail) {
        let mut t = stripped.to_string();
        if t.contains('.') {
            while t.ends_with('0') {
                t.pop();
            }
            if t.ends_with('.') {
                t.pop();
            }
        }
        *s = t;
        s.push_str(tail);
    }
}

enum Task<'a> {
    Draw { bx: &'a MathBox, x: f64, base: f64 },
    Close,
}

pub(crate) fn write(
    tree: &MathBox,
    font: &MathFont,
    fill: Fill,
    colors: bool,
) -> Result<MathSvg, MathError> {
    let width = f(&tree.width)? * MEM;
    let height = f(&tree.height)? * MEM;
    let depth = f(&tree.depth)? * MEM;
    let mut w = Writer {
        font,
        fu_to_mem: MEM / font.units_per_em() as f64,
        colors,
        defs: String::new(),
        body: String::new(),
        used: vec![0u64; (font.glyph_count() as usize).div_ceil(64) + 1],
        min_x: 0.0,
        min_y: -height,
        max_x: width,
        max_y: depth,
    };
    let mut stack = vec![Task::Draw {
        bx: tree,
        x: 0.0,
        base: 0.0,
    }];
    while let Some(task) = stack.pop() {
        let (bx, x, parent_base) = match task {
            Task::Close => {
                w.body.push_str("</g>");
                continue;
            }
            Task::Draw { bx, x, base } => (bx, x, base),
        };
        let base = parent_base - f(&bx.shift)? * MEM;
        match &bx.content {
            BoxContent::Empty | BoxContent::Kern(_) => {}
            BoxContent::Glyph {
                glyph_id, scale, ..
            } => {
                if *glyph_id != MIDDLE_PLACEHOLDER {
                    w.glyph(*glyph_id, x, base, f(scale)?)?;
                }
            }
            BoxContent::Rule => {
                let h = f(&bx.height)? * MEM;
                let d = f(&bx.depth)? * MEM;
                w.rect(x, base - h, f(&bx.width)? * MEM, h + d, "");
            }
            BoxContent::HList(kids) => {
                let mut cx = x;
                let mut placed = Vec::with_capacity(kids.len());
                for k in kids {
                    placed.push(Task::Draw { bx: k, x: cx, base });
                    cx += f(&k.width)? * MEM;
                }
                stack.extend(placed.into_iter().rev());
            }
            BoxContent::VList(kids) => {
                if let Some(first) = kids.first() {
                    let mut placed = vec![Task::Draw { bx: first, x, base }];
                    let mut below = f(&first.depth)? * MEM;
                    for k in &kids[1..] {
                        let kh = f(&k.height)? * MEM;
                        placed.push(Task::Draw {
                            bx: k,
                            x,
                            base: base + below + kh,
                        });
                        below += kh + f(&k.depth)? * MEM;
                    }
                    stack.extend(placed.into_iter().rev());
                }
            }
            BoxContent::Overlap(kids) => {
                stack.extend(kids.iter().rev().map(|k| Task::Draw { bx: k, x, base }));
            }
            BoxContent::Color(c, inner) => {
                if w.colors {
                    let _ = write!(w.body, r#"<g fill="{0}" stroke="{0}">"#, c.css_hex());
                    stack.push(Task::Close);
                }
                stack.push(Task::Draw { bx: inner, x, base });
            }
            BoxContent::BackColor(c, inner) => {
                // In single-color output a filled background would cover the
                // content, so it is only drawn with colors on.
                if w.colors {
                    let h = f(&bx.height)? * MEM;
                    let d = f(&bx.depth)? * MEM;
                    let extra = format!(r#" fill="{}""#, c.css_hex());
                    w.rect(x, base - h, f(&bx.width)? * MEM, h + d, &extra);
                }
                stack.push(Task::Draw { bx: inner, x, base });
            }
            BoxContent::Line {
                x1,
                y1,
                x2,
                y2,
                thickness,
            } => {
                let (x1, y1, x2, y2) = (
                    x + f(x1)? * MEM,
                    base - f(y1)? * MEM,
                    x + f(x2)? * MEM,
                    base - f(y2)? * MEM,
                );
                let t = f(thickness)? * MEM;
                w.extend(x1 - t, y1 - t, x2 + t, y2 + t);
                w.body.push_str("<line x1=\"");
                num(&mut w.body, x1);
                w.body.push_str("\" y1=\"");
                num(&mut w.body, y1);
                w.body.push_str("\" x2=\"");
                num(&mut w.body, x2);
                w.body.push_str("\" y2=\"");
                num(&mut w.body, y2);
                w.body.push_str("\" stroke-width=\"");
                num(&mut w.body, t);
                w.body.push_str("\" stroke-linecap=\"round\"/>");
            }
            BoxContent::Frame {
                thickness,
                stroke,
                inner,
            } => {
                let t = f(thickness)? * MEM;
                let h = f(&bx.height)? * MEM;
                let d = f(&bx.depth)? * MEM;
                let wd = f(&bx.width)? * MEM;
                let mut extra = String::from(" fill=\"none\" stroke-width=\"");
                num(&mut extra, t);
                extra.push('"');
                if let (true, Some(c)) = (w.colors, stroke) {
                    let _ = write!(extra, r#" stroke="{}""#, c.css_hex());
                }
                w.rect(
                    x + t / 2.0,
                    base - h + t / 2.0,
                    (wd - t).max(0.0),
                    (h + d - t).max(0.0),
                    &extra,
                );
                stack.push(Task::Draw { bx: inner, x, base });
            }
        }
    }

    // Pad the ink box a little so antialiased edges are not clipped.
    let pad = 20.0;
    let x0 = (w.min_x - pad).floor();
    let y0 = (w.min_y - pad).floor();
    let x1 = (w.max_x + pad).ceil();
    let y1 = (w.max_y + pad).ceil();
    let (vw, vh) = (x1 - x0, y1 - y0);
    let fill = match fill {
        Fill::CurrentColor => "currentColor".to_string(),
        Fill::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    };
    let mut svg = String::with_capacity(w.defs.len() + w.body.len() + 300);
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{}em" height="{}em" viewBox="{} {} {} {}"><defs>{}</defs><g fill="{fill}" stroke="{fill}" stroke-width="0">{}</g></svg>"#,
        em(vw),
        em(vh),
        x0,
        y0,
        vw,
        vh,
        w.defs,
        w.body
    );
    Ok(MathSvg {
        svg,
        width_em: (vw / MEM) as f32,
        height_em: (vh / MEM) as f32,
        baseline_em: (y1 / MEM) as f32,
        box_width_em: (width / MEM) as f32,
        box_left_em: (-x0 / MEM) as f32,
    })
}

fn em(v: f64) -> String {
    let mut s = format!("{:.3}", v / MEM);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}
