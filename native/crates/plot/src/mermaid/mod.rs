//! Mermaid diagrams (```` ```mermaid ```` fences in Markdown, and
//! `text/vnd.mermaid` outputs) drawn as static SVG, without a browser.
//!
//! Supported: flowcharts (`graph` / `flowchart`), sequence diagrams, pie
//! charts, state diagrams, class diagrams and ER diagrams. Node-and-edge diagrams share a layered layout
//! (see [`layout`]). Anything else, or syntax this module doesn't
//! understand, returns `None` so the caller shows the source instead: a
//! wrong diagram is worse than none.
//!
//! The look is our own, calm and minimal (neutral fills, 1 px strokes,
//! rounded corners), not mermaid's default theme.

mod class;
mod er;
mod flowchart;
mod graph;
mod layout;
mod ns;
mod pie;
mod sequence;
mod state;

use std::fmt::Write as _;

use crate::color::Rgba;
use crate::svg::{self, Svg};
use crate::text;
use crate::{RenderOptions, Rendered};

/// Inputs larger than this are declined (a notebook diagram is a few KB).
const MAX_SOURCE: usize = 200_000;

/// A drawn diagram body, in its own coordinates from (0, 0).
pub(crate) struct Doc {
    pub body: String,
    pub w: f64,
    pub h: f64,
    /// A title the diagram itself declares (`pie title …`).
    pub title: Option<String>,
    pub notes: Vec<String>,
}

pub fn render(source: &str, opts: &RenderOptions) -> Option<Rendered> {
    if source.len() > MAX_SOURCE {
        return None;
    }
    let src = Source::parse(source)?;
    let th = Theme::new(opts.dark);
    let doc = match keyword(&src.lines) {
        "sequenceDiagram" => sequence::render(&src.lines, &th)?,
        "pie" => pie::render(&src.lines, &th)?,
        _ => {
            let (g, notes) = graph_of(&src.lines)?;
            let mut d = graph::render(&g, &th)?;
            d.notes = notes;
            d
        }
    };
    if !(doc.w.is_finite() && doc.h.is_finite() && doc.w > 0. && doc.h > 0. && doc.w < 1e5 && doc.h < 1e5) {
        return None;
    }
    Some(assemble(doc, src.title, opts, &th))
}

/// The diagram type: the header's first word.
fn keyword(lines: &[String]) -> &str {
    let header = lines.first().map_or("", |l| l.trim());
    header.split(|c: char| c.is_whitespace() || c == ';' || c == '{').next().unwrap_or("")
}

/// Node-and-edge diagrams, parsed (with notes on what was left out).
fn graph_of(lines: &[String]) -> Option<(graph::Graph, Vec<String>)> {
    match keyword(lines) {
        "graph" | "flowchart" | "flowchart-elk" => Some((flowchart::parse(lines)?, Vec::new())),
        "stateDiagram" | "stateDiagram-v2" => state::parse(lines),
        "classDiagram" | "classDiagram-v2" => class::parse(lines),
        "erDiagram" => er::parse(lines),
        _ => None,
    }
}

/// Title, margins and scaling to the available width.
fn assemble(doc: Doc, front_title: Option<String>, opts: &RenderOptions, th: &Theme) -> Rendered {
    let margin = 8.;
    let title = doc.title.clone().or(front_title).filter(|t| !t.trim().is_empty());
    let title_size = 16.;
    let (title_w, title_h) = match &title {
        Some(t) => (text::width(t, title_size, true), title_size * LINE + 10.),
        None => (0., 0.),
    };
    let vw = doc.w.max(title_w) + 2. * margin;
    let vh = doc.h + title_h + 2. * margin;
    let scale = (opts.width as f64 / vw).min(1.);
    let (w, h) = ((vw * scale * 100.).round() / 100., (vh * scale * 100.).round() / 100.);
    let mut s = Svg::with_view(w, h, vw, vh, &opts.font_family, th.font);
    if let Some(t) = &title {
        text_line(&mut s.out, vw / 2., margin + title_size * LINE / 2., t, title_size, th.text, 600, svg::Anchor::Middle);
    }
    let dx = margin + (vw - 2. * margin - doc.w) / 2.;
    let _ = writeln!(s.out, "<g transform=\"translate({} {})\">", svg::fmt_num(dx), svg::fmt_num(margin + title_h));
    s.out.push_str(&doc.body);
    s.out.push_str("</g>\n");
    Rendered { svg: s.finish(), width: w as f32, height: h as f32, notes: doc.notes }
}

/// The source with frontmatter, `%%{init}%%` directives and `%%` comments
/// removed; `lines[0]` is the diagram header.
struct Source {
    lines: Vec<String>,
    title: Option<String>,
}

impl Source {
    fn parse(source: &str) -> Option<Source> {
        let text = source.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
        let mut title = None;
        let mut rest: &str = &text;
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix("---")
            && after.starts_with(['\n', ' ', '\t'])
        {
            let after = after.trim_start_matches([' ', '\t']);
            let after = after.strip_prefix('\n')?;
            let mut end = None;
            let mut off = 0;
            for line in after.split_inclusive('\n') {
                if line.trim() == "---" {
                    end = Some(off + line.len());
                    break;
                }
                if let Some(t) = line.strip_prefix("title:") {
                    let t = t.trim();
                    let quoted = t.len() >= 2 && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')));
                    title = Some(if quoted { &t[1..t.len() - 1] } else { t }.to_string());
                }
                off += line.len();
            }
            rest = &after[end?..];
        }
        // `%%{ ... }%%` directives may span lines.
        let mut cleaned = String::with_capacity(rest.len());
        let mut r = rest;
        while let Some(i) = r.find("%%{") {
            cleaned.push_str(&r[..i]);
            let j = r[i..].find("}%%")?;
            r = &r[i + j + 3..];
        }
        cleaned.push_str(r);
        let lines: Vec<String> = cleaned
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| {
                let t = l.trim_start();
                !t.is_empty() && !t.starts_with("%%")
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        Some(Source { lines, title })
    }
}

// ---------------------------------------------------------------- theme

/// Colours for the app's light page (`#FFFFFF`) or dark page (`#191817`).
pub(crate) struct Theme {
    pub dark: bool,
    /// The page colour, for pads under edge labels.
    pub bg: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub node_fill: Rgba,
    pub node_stroke: Rgba,
    pub edge: Rgba,
    pub cluster_fill: Rgba,
    pub cluster_stroke: Rgba,
    pub note_fill: Rgba,
    pub note_stroke: Rgba,
    /// Activation bars, block tags.
    pub tint: Rgba,
    /// Categorical colours (pie slices).
    pub series: [Rgba; 8],
    /// Body text size.
    pub font: f64,
    /// Edge labels, cluster titles, captions.
    pub small: f64,
}

impl Theme {
    pub fn new(dark: bool) -> Theme {
        let c = Rgba::hex_rgb;
        if dark {
            Theme {
                dark,
                bg: c(0x191817),
                text: c(0xECEAE6),
                muted: c(0xA29C95),
                node_fill: c(0x24221F),
                node_stroke: c(0x6E6862),
                edge: c(0x8F8982),
                cluster_fill: c(0x1D1C1A),
                cluster_stroke: c(0x3A3734),
                note_fill: c(0x2B281F),
                note_stroke: c(0x5A5139),
                tint: c(0x2E2B28),
                series: [c(0x3987E5), c(0xD95926), c(0x199E70), c(0xC98500), c(0xD55181), c(0x008300), c(0x9085E9), c(0xE66767)],
                font: 14.,
                small: 12.5,
            }
        } else {
            Theme {
                dark,
                bg: c(0xFFFFFF),
                text: c(0x1C1A17),
                muted: c(0x6B665F),
                node_fill: c(0xF5F4F1),
                node_stroke: c(0x9A958D),
                edge: c(0x7A756E),
                cluster_fill: c(0xFAF9F7),
                cluster_stroke: c(0xDDDAD4),
                note_fill: c(0xFBF6E6),
                note_stroke: c(0xE2D5AE),
                tint: c(0xEDEBE7),
                series: [c(0x2A78D6), c(0xEB6834), c(0x1BAF7A), c(0xEDA100), c(0xE87BA4), c(0x008300), c(0x4A3AA7), c(0xE34948)],
                font: 14.,
                small: 12.5,
            }
        }
    }
}

// ---------------------------------------------------------------- text

/// Line height, in ems.
pub(crate) const LINE: f64 = 1.3;

/// Width and height of a block of lines.
pub(crate) fn measure(lines: &[String], size: f64, bold: bool) -> (f64, f64) {
    let w = lines.iter().map(|l| text::width(l, size, bold)).fold(0., f64::max);
    (w, lines.len().max(1) as f64 * size * LINE)
}

/// One line of text whose vertical centre is `cy`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn text_line(out: &mut String, x: f64, cy: f64, s: &str, size: f64, color: Rgba, weight: u16, anchor: svg::Anchor) {
    if s.is_empty() {
        return;
    }
    out.push_str("<text x=\"");
    svg::num(out, x);
    out.push_str("\" y=\"");
    // Cap height is ~0.7 em: this puts capitals and digits on the centre.
    svg::num(out, cy + 0.35 * size);
    out.push('"');
    if (size - 14.).abs() > 1e-9 {
        out.push_str(" font-size=\"");
        svg::num(out, size);
        out.push('"');
    }
    if weight != 400 {
        let _ = write!(out, " font-weight=\"{weight}\"");
    }
    match anchor {
        svg::Anchor::Start => {}
        svg::Anchor::Middle => out.push_str(" text-anchor=\"middle\""),
        svg::Anchor::End => out.push_str(" text-anchor=\"end\""),
    }
    svg::paint(out, "fill", Some(color));
    out.push('>');
    svg::esc(out, s);
    out.push_str("</text>\n");
}

/// Lines centred as a block on (`x`, `cy`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn text_block(out: &mut String, x: f64, cy: f64, lines: &[String], size: f64, color: Rgba, weight: u16, anchor: svg::Anchor) {
    let lh = size * LINE;
    let top = cy - lh * lines.len() as f64 / 2.;
    for (i, l) in lines.iter().enumerate() {
        text_line(out, x, top + lh * (i as f64 + 0.5), l, size, color, weight, anchor);
    }
}

/// A label as mermaid writes it — maybe quoted, maybe a Markdown string
/// (`` "`…`" ``), with `<br>`, HTML tags, `&amp;`/`#35;` entities and
/// Font Awesome icons — as plain lines.
pub(crate) fn label_lines(raw: &str, font: f64) -> Vec<String> {
    let mut s = raw.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s = &s[1..s.len() - 1];
    }
    let markdown = s.len() >= 2 && s.starts_with('`') && s.ends_with('`');
    if markdown {
        s = &s[1..s.len() - 1];
    }
    let s = mermaid_entities(s);
    let mut lines = Vec::new();
    let paragraphs: Vec<&str> = if markdown { s.split('\n').collect() } else { vec![s.as_str()] };
    for p in paragraphs {
        for runs in text::rich(p) {
            let mut line: String = runs.iter().map(|r| r.text.as_str()).collect();
            if markdown {
                line = strip_markdown(&line);
            }
            let line = strip_icons(&line);
            let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
            if markdown {
                lines.extend(wrap(&line, font, 200.));
            } else {
                lines.push(line);
            }
        }
    }
    while lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// mermaid's `#quot;` / `#35;` entity syntax, as HTML entities.
fn mermaid_entities(s: &str) -> String {
    if !s.contains('#') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('#') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let end = tail[..tail.len().min(10)].find(';');
        match end {
            Some(e) if e > 0 && tail[..e].chars().all(|c| c.is_ascii_alphanumeric()) => {
                let ent = &tail[..e];
                if ent.chars().all(|c| c.is_ascii_digit()) {
                    out.push_str(&format!("&#{ent};"));
                } else {
                    out.push_str(&format!("&{ent};"));
                }
                rest = &tail[e + 1..];
            }
            _ => {
                out.push('#');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

fn strip_markdown(s: &str) -> String {
    let s = s.replace("**", "").replace("__", "");
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '*' || c == '_' {
            let before = i == 0 || !chars[i - 1].is_alphanumeric();
            let after = i + 1 >= chars.len() || !chars[i + 1].is_alphanumeric();
            if before || after {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Drops `fa:fa-car`-style icon references.
fn strip_icons(s: &str) -> String {
    if !s.contains(":fa-") {
        return s.to_string();
    }
    s.split(' ')
        .filter(|w| !(w.contains(":fa-") && w.split(':').next().is_some_and(|p| matches!(p, "fa" | "fab" | "fas" | "far" | "fal" | "fak"))))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Greedy word wrap at `max` px.
pub(crate) fn wrap(s: &str, size: f64, max: f64) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && text::width(&format!("{cur} {word}"), size, false) > max {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(label_lines("\"text with (parens)\"", 14.), vec!["text with (parens)"]);
        assert_eq!(label_lines("one<br>two<br/>three<br />four", 14.), vec!["one", "two", "three", "four"]);
        assert_eq!(label_lines("A #quot;quoted#quot; #35;1 &amp; co", 14.), vec!["A \"quoted\" #1 & co"]);
        assert_eq!(label_lines("\"`**Bold** and _it_`\"", 14.), vec!["Bold and it"]);
        assert_eq!(label_lines("fa:fa-car Car", 14.), vec!["Car"]);
        assert_eq!(label_lines("<b>x</b> y", 14.), vec!["x y"]);
        let long = label_lines("\"`a markdown string long enough that it has to wrap onto more lines`\"", 14.);
        assert!(long.len() >= 2, "{long:?}");
    }

    #[test]
    fn source_preprocessing() {
        let s = Source::parse("---\ntitle: Hello \"x\"\nconfig:\n  theme: dark\n---\n%%{init: {'theme':'dark'}}%%\n%% a comment\ngraph TD\n  A-->B\n").unwrap();
        assert_eq!(s.title.as_deref(), Some("Hello \"x\""));
        assert_eq!(s.lines, vec!["graph TD", "  A-->B"]);
        let s = Source::parse("%%{\n init: {}\n}%%\nflowchart LR\nA").unwrap();
        assert_eq!(s.lines[0], "flowchart LR");
        assert!(Source::parse("---\ntitle: x\n").is_none());
        assert!(Source::parse("  \n%% only a comment\n").is_none());
    }
}
