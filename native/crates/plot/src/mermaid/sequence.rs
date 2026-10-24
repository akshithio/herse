//! `sequenceDiagram`: participants and actors, messages, notes, activation
//! bars, `loop`/`alt`/`opt`/`par`/`critical`/`break`/`rect` blocks,
//! `box` groups and `autonumber`.
//!
//! Participants sit in a row (and again at the bottom, as mermaid draws
//! them), spaced so every message label and note fits; events then stack
//! downwards.

use super::{Doc, LINE, Theme, label_lines, measure, text_block, text_line};
use crate::color::Rgba;
use crate::svg::{self, Anchor, PathData, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tip {
    None,
    Filled,
    Open,
    Cross,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Arrow {
    dotted: bool,
    tip: Tip,
    both: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotePos {
    Left,
    Right,
    Over,
}

#[derive(Clone, Debug)]
enum Ev {
    Msg { from: usize, to: usize, arrow: Arrow, text: Vec<String>, activate: bool, deactivate: bool, num: Option<u32> },
    Note { pos: NotePos, a: usize, b: usize, text: Vec<String> },
    Activate(usize),
    Deactivate(usize),
    Start { kind: &'static str, label: Vec<String>, fill: Option<Rgba> },
    Sep { label: Vec<String> },
    End,
}

#[derive(Clone, Debug)]
struct Participant {
    id: String,
    label: Vec<String>,
    actor: bool,
    w: f64,
    h: f64,
    x: f64,
}

struct Group {
    label: Vec<String>,
    members: Vec<usize>,
}

#[derive(Default)]
struct Parsed {
    parts: Vec<Participant>,
    events: Vec<Ev>,
    groups: Vec<Group>,
    title: Option<String>,
    /// Open `box` groups and blocks, innermost last (`true` = box).
    stack: Vec<bool>,
    autonumber: Option<(u32, u32)>,
}

const ARROWS: &[(&str, Arrow)] = &[
    ("<<-->>", Arrow { dotted: true, tip: Tip::Filled, both: true }),
    ("<<->>", Arrow { dotted: false, tip: Tip::Filled, both: true }),
    ("-->>", Arrow { dotted: true, tip: Tip::Filled, both: false }),
    ("->>", Arrow { dotted: false, tip: Tip::Filled, both: false }),
    ("--x", Arrow { dotted: true, tip: Tip::Cross, both: false }),
    ("-x", Arrow { dotted: false, tip: Tip::Cross, both: false }),
    ("--)", Arrow { dotted: true, tip: Tip::Open, both: false }),
    ("-)", Arrow { dotted: false, tip: Tip::Open, both: false }),
    ("-->", Arrow { dotted: true, tip: Tip::None, both: false }),
    ("->", Arrow { dotted: false, tip: Tip::None, both: false }),
];

impl Parsed {
    fn participant(&mut self, id: &str, th: &Theme) -> Option<usize> {
        let id = id.trim();
        if id.is_empty() || id.contains([':', ',', '"']) {
            return None;
        }
        if let Some(i) = self.parts.iter().position(|p| p.id == id) {
            return Some(i);
        }
        let label = label_lines(id, th.font);
        self.parts.push(Participant { id: id.to_string(), label, actor: false, w: 0., h: 0., x: 0. });
        Some(self.parts.len() - 1)
    }

    fn line(&mut self, l: &str, th: &Theme) -> Option<()> {
        if let Some(t) = l.strip_prefix("title:") {
            self.title = Some(t.trim().to_string());
            return Some(());
        }
        let (word, rest) = match l.find(char::is_whitespace) {
            Some(i) => (&l[..i], l[i..].trim()),
            None => (l, ""),
        };
        let small = th.small;
        match word {
            "participant" | "actor" => {
                if rest.contains("@{") {
                    return None;
                }
                let (id, alias) = match rest.find(" as ") {
                    Some(i) => (&rest[..i], Some(rest[i + 4..].trim())),
                    None => (rest, None),
                };
                let i = self.participant(id, th)?;
                if let Some(a) = alias {
                    self.parts[i].label = label_lines(a, th.font);
                }
                self.parts[i].actor = word == "actor";
                if self.stack.last() == Some(&true)
                    && let Some(g) = self.groups.last_mut()
                    && !g.members.contains(&i)
                {
                    g.members.push(i);
                }
            }
            "create" | "destroy" => return None,
            "box" => {
                let mut label = rest;
                // An optional colour (ignored: groups use the theme's tint).
                if ["rgb(", "rgba(", "hsl(", "hsla("].iter().any(|p| rest.starts_with(p)) {
                    label = rest[rest.find(')')? + 1..].trim();
                } else if let Some(first) = rest.split_whitespace().next()
                    && Rgba::parse(first).is_some()
                {
                    label = rest[first.len()..].trim();
                }
                self.groups.push(Group { label: if label.is_empty() { Vec::new() } else { label_lines(label, small) }, members: Vec::new() });
                self.stack.push(true);
            }
            "autonumber" => {
                if rest == "off" {
                    self.autonumber = None;
                } else {
                    let nums: Vec<u32> = rest.split_whitespace().map(|n| n.parse().ok()).collect::<Option<_>>()?;
                    self.autonumber = Some((nums.first().copied().unwrap_or(1), nums.get(1).copied().unwrap_or(1)));
                }
            }
            "title" => self.title = Some(rest.to_string()),
            "activate" => {
                let i = self.participant(rest, th)?;
                self.events.push(Ev::Activate(i));
            }
            "deactivate" => {
                let i = self.participant(rest, th)?;
                self.events.push(Ev::Deactivate(i));
            }
            "loop" | "alt" | "opt" | "par" | "critical" | "break" => {
                let kind = match word {
                    "loop" => "loop",
                    "alt" => "alt",
                    "opt" => "opt",
                    "par" => "par",
                    "critical" => "critical",
                    _ => "break",
                };
                let label = if rest.is_empty() { Vec::new() } else { label_lines(rest, small) };
                self.events.push(Ev::Start { kind, label, fill: None });
                self.stack.push(false);
            }
            "rect" => {
                let fill = Rgba::parse(rest)?;
                self.events.push(Ev::Start { kind: "rect", label: Vec::new(), fill: Some(fill) });
                self.stack.push(false);
            }
            "else" | "and" | "option" => {
                if self.stack.last() != Some(&false) {
                    return None;
                }
                let label = if rest.is_empty() { Vec::new() } else { label_lines(rest, small) };
                self.events.push(Ev::Sep { label });
            }
            "end" if rest.is_empty() => {
                if !self.stack.pop()? {
                    self.events.push(Ev::End);
                }
            }
            "links" | "link" | "properties" | "details" => {}
            _ if word.eq_ignore_ascii_case("note") => return self.note(rest, th),
            _ => return self.message(l, th),
        }
        Some(())
    }

    fn note(&mut self, rest: &str, th: &Theme) -> Option<()> {
        let colon = rest.find(':')?;
        let (place, text) = (rest[..colon].trim(), rest[colon + 1..].trim());
        let (pos, who) = if let Some(w) = place.strip_prefix("left of ") {
            (NotePos::Left, w)
        } else if let Some(w) = place.strip_prefix("right of ") {
            (NotePos::Right, w)
        } else {
            (NotePos::Over, place.strip_prefix("over ")?)
        };
        let mut ids = who.split(',');
        let a = self.participant(ids.next()?, th)?;
        let b = match ids.next() {
            Some(id) if pos == NotePos::Over => self.participant(id, th)?,
            Some(_) => return None,
            None => a,
        };
        if ids.next().is_some() {
            return None;
        }
        let (a, b) = (a.min(b), a.max(b));
        self.events.push(Ev::Note { pos, a, b, text: label_lines(text, th.small) });
        Some(())
    }

    fn message(&mut self, l: &str, th: &Theme) -> Option<()> {
        // The earliest arrow, longest first at that position.
        let mut found: Option<(usize, &str, Arrow)> = None;
        for &(pat, arrow) in ARROWS {
            if let Some(i) = l.find(pat)
                && found.is_none_or(|(j, p, _)| i < j || (i == j && pat.len() > p.len()))
            {
                found = Some((i, pat, arrow));
            }
        }
        let (i, pat, arrow) = found?;
        let from = self.participant(&l[..i], th)?;
        let mut rest = &l[i + pat.len()..];
        let mut activate = false;
        let mut deactivate = false;
        if let Some(r) = rest.strip_prefix('+') {
            activate = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('-') {
            deactivate = true;
            rest = r;
        }
        let (to, text) = match rest.find(':') {
            Some(c) => (&rest[..c], rest[c + 1..].trim()),
            None => (rest, ""),
        };
        let to = self.participant(to, th)?;
        let num = self.autonumber.map(|(n, step)| {
            self.autonumber = Some((n.saturating_add(step), step));
            n
        });
        let text = if text.is_empty() { Vec::new() } else { label_lines(text, th.font) };
        self.events.push(Ev::Msg { from, to, arrow, text, activate, deactivate, num });
        Some(())
    }
}

/// Between neighbouring participant boxes.
const GAP: f64 = 40.;
/// Activation bar width.
const BAR: f64 = 10.;
/// How far a self-message loops out.
const LOOP_W: f64 = 34.;
const NOTE_PAD: f64 = 8.;
/// Between a block's frame and what it holds.
const BLOCK_PAD: f64 = 10.;

/// Drawn output in layers, and the extent of what's been drawn.
struct Canvas {
    back: String,
    lines: String,
    frames: String,
    front: String,
    boxes: String,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Canvas {
    fn grow(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        self.x0 = self.x0.min(x0);
        self.y0 = self.y0.min(y0);
        self.x1 = self.x1.max(x1);
        self.y1 = self.y1.max(y1);
    }
}

struct OpenBlock {
    kind: &'static str,
    label: Vec<String>,
    fill: Option<Rgba>,
    top: f64,
    x0: f64,
    x1: f64,
    seps: Vec<(f64, Vec<String>)>,
}

pub fn render(lines: &[String], th: &Theme) -> Option<Doc> {
    if lines.first()?.trim() != "sequenceDiagram" {
        return None;
    }
    let mut p = Parsed::default();
    for l in &lines[1..] {
        let l = l.trim();
        if !l.is_empty() {
            p.line(l, th)?;
        }
    }
    if !p.stack.is_empty() || p.parts.is_empty() || p.parts.len() > 200 || p.events.len() > 5000 {
        return None;
    }
    let font = th.font;
    let small = th.small;

    // Participant sizes and spacing.
    for part in p.parts.iter_mut() {
        let (tw, tht) = measure(&part.label, font, false);
        if part.actor {
            part.w = (tw + 8.).max(44.);
            part.h = 36. + tht;
        } else {
            part.w = (tw + 28.).max(72.);
            part.h = tht + 20.;
        }
    }
    let n = p.parts.len();
    let mut x = vec![0f64; n];
    for i in 1..n {
        x[i] = x[i - 1] + p.parts[i - 1].w / 2. + GAP + p.parts[i].w / 2.;
    }
    // (left, right, needed distance between their lifelines)
    let mut needs: Vec<(usize, usize, f64)> = Vec::new();
    let mut right_extra = 0f64;
    let mut left_extra = 0f64;
    for ev in &p.events {
        match ev {
            Ev::Msg { from, to, text, .. } => {
                let (tw, _) = measure(text, font, false);
                if from == to {
                    let need = (tw + 16.).max(LOOP_W + 16.);
                    if *from + 1 < n {
                        needs.push((*from, from + 1, need));
                    } else {
                        right_extra = right_extra.max(need);
                    }
                } else {
                    needs.push(((*from).min(*to), (*from).max(*to), tw + 28.));
                }
            }
            Ev::Note { pos, a, b, text } => {
                let (tw, _) = measure(text, small, false);
                let nw = tw + 2. * NOTE_PAD;
                match pos {
                    NotePos::Right if *a + 1 < n => needs.push((*a, a + 1, nw + 2. * BAR + 8.)),
                    NotePos::Right => right_extra = right_extra.max(nw + BAR + 8.),
                    NotePos::Left if *a > 0 => needs.push((a - 1, *a, nw + 2. * BAR + 8.)),
                    NotePos::Left => left_extra = left_extra.max(nw + BAR + 8.),
                    NotePos::Over if a != b => needs.push((*a, *b, nw - 40.)),
                    NotePos::Over => {}
                }
            }
            _ => {}
        }
    }
    needs.sort_by_key(|&(a, b, _)| (b - a, a));
    for (a, b, need) in needs {
        let have = x[b] - x[a];
        if have < need {
            for xi in x.iter_mut().skip(b) {
                *xi += need - have;
            }
        }
    }
    for (i, part) in p.parts.iter_mut().enumerate() {
        part.x = x[i];
    }

    let mut cv = Canvas { back: String::new(), lines: String::new(), frames: String::new(), front: String::new(), boxes: String::new(), x0: 0., y0: 0., x1: 0., y1: 0. };
    cv.grow(x[0] - p.parts[0].w / 2. - left_extra, 0., x[n - 1] + p.parts[n - 1].w / 2. + right_extra, 0.);
    let group_label_h = if p.groups.iter().any(|g| !g.label.is_empty()) { small * LINE + 8. } else if p.groups.is_empty() { 0. } else { 6. };
    let top = group_label_h;
    let head_h = p.parts.iter().map(|q| q.h).fold(0., f64::max);
    let mut y = top + head_h + 24.;

    // Activation bars: per participant, a stack of start ys.
    let mut active: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut bars: Vec<(usize, usize, f64, f64)> = Vec::new();
    let mut last_line: Option<f64> = None;
    let mut blocks: Vec<OpenBlock> = Vec::new();
    let edge_x = |i: usize, depth: usize, toward_right: bool| -> f64 {
        if depth == 0 {
            x[i]
        } else {
            let c = x[i] + 4. * (depth - 1) as f64;
            if toward_right { c + BAR / 2. } else { c - BAR / 2. }
        }
    };
    let grow_block = |blocks: &mut Vec<OpenBlock>, a: f64, b: f64| {
        if let Some(bl) = blocks.last_mut() {
            bl.x0 = bl.x0.min(a);
            bl.x1 = bl.x1.max(b);
        }
    };
    let line_style = |dotted: bool| Style {
        stroke: Some(th.edge),
        stroke_width: 1.,
        dash: dotted.then(|| "4,3".to_string()),
        opacity: 1.,
        round_joins: !dotted,
        ..Default::default()
    };
    for ev in &p.events {
        match ev {
            Ev::Msg { from, to, arrow, text, activate, deactivate, num } => {
                let (from, to) = (*from, *to);
                let (_, tht) = if text.is_empty() { (0., 0.) } else { measure(text, font, false) };
                y += tht;
                let ly = y + 7.;
                if *activate {
                    active[to].push(ly);
                }
                if from == to {
                    let d = active[from].len();
                    let sx = edge_x(from, d, true);
                    let (y1, y2) = (ly, ly + 20.);
                    let mut d = PathData::new();
                    d.move_to(sx, y1);
                    d.cubic_to((sx + LOOP_W * 1.3, y1), (sx + LOOP_W * 1.3, y2), (sx + 2., y2));
                    let st = line_style(arrow.dotted);
                    path(&mut cv.front, &d.d, &st);
                    tip(&mut cv.front, arrow.tip, (sx, y2), (-1., 0.), th);
                    let tx = sx + if num.is_some() { 14. } else { 8. };
                    text_block(&mut cv.front, tx, y1 - 6. - tht / 2., text, font, th.text, 400, Anchor::Start);
                    let (tw, _) = measure(text, font, false);
                    grow_block(&mut blocks, x[from] - 4., sx + (tw + 6.).max(LOOP_W));
                    cv.grow(x[from], y1 - tht, sx + (tw + 8.).max(LOOP_W + 4.), y2);
                    if let Some(k) = num {
                        number(&mut cv.front, (x[from], y1), *k, th);
                    }
                    last_line = Some(y2);
                    y = y2 + 18.;
                } else {
                    let right = x[to] > x[from];
                    let sx = edge_x(from, active[from].len(), right);
                    let ex = edge_x(to, active[to].len(), !right);
                    let dir = if right { 1. } else { -1. };
                    let tip_len = if arrow.tip == Tip::None { 0. } else { 8. };
                    let start = if arrow.both { sx + dir * tip_len } else { sx };
                    let mut d = PathData::new();
                    d.move_to(start, ly);
                    d.line_to(ex - dir * if arrow.tip == Tip::Filled { tip_len } else { 0. }, ly);
                    path(&mut cv.front, &d.d, &line_style(arrow.dotted));
                    tip(&mut cv.front, arrow.tip, (ex, ly), (dir, 0.), th);
                    if arrow.both {
                        tip(&mut cv.front, arrow.tip, (sx, ly), (-dir, 0.), th);
                    }
                    text_block(&mut cv.front, (sx + ex) / 2., ly - 7. - tht / 2., text, font, th.text, 400, Anchor::Middle);
                    if let Some(k) = num {
                        number(&mut cv.front, (sx, ly), *k, th);
                    }
                    grow_block(&mut blocks, sx.min(ex), sx.max(ex));
                    last_line = Some(ly);
                    y = ly + 18.;
                }
                if *deactivate && let Some(s) = active[from].pop() {
                    bars.push((from, active[from].len(), s, last_line.unwrap_or(y)));
                }
            }
            Ev::Note { pos, a, b, text } => {
                let (tw, tht) = measure(text, small, false);
                let w = tw + 2. * NOTE_PAD;
                let h = tht + 12.;
                let ny = y + 2.;
                let nx = match pos {
                    NotePos::Right => x[*a] + BAR / 2. + 4.,
                    NotePos::Left => x[*a] - BAR / 2. - 4. - w,
                    NotePos::Over => {
                        let (l, r) = (x[*a], x[*b]);
                        let span = if a == b { w.max(64.) } else { (r - l + 40.).max(w) };
                        (l + r) / 2. - span / 2.
                    }
                };
                let w = match pos {
                    NotePos::Over if a == b => w.max(64.),
                    NotePos::Over => (x[*b] - x[*a] + 40.).max(w),
                    _ => w,
                };
                let st = Style { fill: Some(th.note_fill), stroke: Some(th.note_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
                rect(&mut cv.front, nx + 0.5, ny + 0.5, w - 1., h - 1., 4., &st);
                text_block(&mut cv.front, nx + w / 2., ny + h / 2., text, small, th.text, 400, Anchor::Middle);
                grow_block(&mut blocks, nx, nx + w);
                cv.grow(nx, ny, nx + w, ny + h);
                y = ny + h + 16.;
            }
            Ev::Activate(i) => active[*i].push(last_line.unwrap_or(y)),
            Ev::Deactivate(i) => {
                if let Some(s) = active[*i].pop() {
                    let end = last_line.filter(|&l| l > s).unwrap_or(y - 8.);
                    bars.push((*i, active[*i].len(), s, end.max(s + 8.)));
                }
            }
            Ev::Start { kind, label, fill } => {
                y += 6.;
                blocks.push(OpenBlock { kind, label: label.clone(), fill: *fill, top: y, x0: f64::INFINITY, x1: f64::NEG_INFINITY, seps: Vec::new() });
                y += if *kind == "rect" { 10. } else { small * LINE + 16. };
                last_line = None;
            }
            Ev::Sep { label } => {
                y += 4.;
                let sy = y;
                if let Some(bl) = blocks.last_mut() {
                    bl.seps.push((sy, label.clone()));
                }
                y += small * LINE + 12.;
                last_line = None;
            }
            Ev::End => {
                let bl = blocks.pop()?;
                y += 6.;
                let (mut x0, mut x1) = if bl.x0.is_finite() { (bl.x0 - BLOCK_PAD - 8., bl.x1 + BLOCK_PAD + 8.) } else { (x[0] - 40., x[0] + 40.) };
                let tag = if bl.kind == "rect" { 0. } else { svg_width(bl.kind, small, true) + 16. };
                let cond = bl.label.iter().map(|l| svg_width(&format!("[{l}]"), small, false)).fold(0., f64::max);
                let need = tag + cond + 24.;
                if x1 - x0 < need {
                    x1 = x0 + need;
                }
                for (_, l) in &bl.seps {
                    let w = l.iter().map(|s| svg_width(&format!("[{s}]"), small, false)).fold(0., f64::max) + 24.;
                    if x1 - x0 < w {
                        let d = (w - (x1 - x0)) / 2.;
                        x0 -= d;
                        x1 += d;
                    }
                }
                draw_block(&mut cv, &bl, x0, x1, y, th);
                cv.grow(x0, bl.top, x1, y);
                let (a, b) = (x0, x1);
                if let Some(parent) = blocks.last_mut() {
                    parent.x0 = parent.x0.min(a);
                    parent.x1 = parent.x1.max(b);
                }
                y += 10.;
                last_line = None;
            }
        }
    }
    let bottom = y + 8.;
    for (i, st) in active.iter().enumerate() {
        for (d, &s) in st.iter().enumerate() {
            bars.push((i, d, s, bottom - 8.));
        }
    }
    // Lifelines, then activation bars over them.
    let life = Style { stroke: Some(th.cluster_stroke.with_alpha(1.)), stroke_width: 1., dash: Some("3,3".into()), opacity: 1., ..Default::default() };
    let life = if th.dark { Style { stroke: Some(th.node_stroke.scale_alpha(0.7)), ..life } } else { Style { stroke: Some(th.node_stroke.scale_alpha(0.55)), ..life } };
    for part in &p.parts {
        let mut d = PathData::new();
        d.move_to(part.x, top + part.h);
        d.line_to(part.x, bottom);
        path(&mut cv.lines, &d.d, &life);
    }
    bars.sort_by_key(|b| b.1);
    let bar_style = Style { fill: Some(th.tint), stroke: Some(th.node_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
    for (i, depth, s, e) in bars {
        let cx = x[i] + 4. * depth as f64;
        rect(&mut cv.lines, cx - BAR / 2. + 0.5, s, BAR - 1., (e - s).max(6.), 1.5, &bar_style);
    }
    // Participant boxes, top and bottom.
    for part in &p.parts {
        participant(&mut cv.boxes, part, top, th);
        participant(&mut cv.boxes, part, bottom, th);
        cv.grow(part.x - part.w / 2., 0., part.x + part.w / 2., bottom + part.h);
    }
    // Box groups behind everything.
    for g in &p.groups {
        if g.members.is_empty() {
            continue;
        }
        let gx0 = g.members.iter().map(|&i| x[i] - p.parts[i].w / 2.).fold(f64::INFINITY, f64::min) - 10.;
        let gx1 = g.members.iter().map(|&i| x[i] + p.parts[i].w / 2.).fold(f64::NEG_INFINITY, f64::max) + 10.;
        let gy1 = bottom + head_h + 10.;
        let st = Style { fill: Some(th.cluster_fill), stroke: Some(th.cluster_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
        let mut s = String::new();
        rect(&mut s, gx0 + 0.5, 0.5, gx1 - gx0 - 1., gy1 - 1., 8., &st);
        text_block(&mut s, (gx0 + gx1) / 2., 4. + small * LINE / 2., &g.label, small, th.muted, 600, Anchor::Middle);
        s.push_str(&cv.back);
        cv.back = s;
        cv.grow(gx0, 0., gx1, gy1);
    }
    let (w, h) = (cv.x1 - cv.x0, cv.y1 - cv.y0);
    let mut body = String::with_capacity(cv.back.len() + cv.lines.len() + cv.frames.len() + cv.front.len() + cv.boxes.len() + 64);
    body.push_str("<g transform=\"translate(");
    svg::num(&mut body, -cv.x0);
    body.push(' ');
    svg::num(&mut body, -cv.y0);
    body.push_str(")\">\n");
    for layer in [&cv.back, &cv.lines, &cv.frames, &cv.front, &cv.boxes] {
        body.push_str(layer);
    }
    body.push_str("</g>\n");
    Some(Doc { body, w, h, title: p.title, notes: Vec::new() })
}

fn svg_width(s: &str, size: f64, bold: bool) -> f64 {
    crate::text::width(s, size, bold)
}

fn draw_block(cv: &mut Canvas, bl: &OpenBlock, x0: f64, x1: f64, y1: f64, th: &Theme) {
    let small = th.small;
    if let Some(fill) = bl.fill {
        let a = fill.a as f64 * if th.dark { 0.22 } else { 0.5 };
        let mut s = String::new();
        rect(&mut s, x0, bl.top, x1 - x0, y1 - bl.top, 6., &Style::fill(fill.with_alpha(a)));
        cv.back.push_str(&s);
        return;
    }
    let o = &mut cv.frames;
    let frame = Style { stroke: Some(th.node_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
    rect(o, x0 + 0.5, bl.top + 0.5, x1 - x0 - 1., y1 - bl.top - 1., 6., &frame);
    // The keyword in a tab at the top left, the condition beside it.
    let tag_w = svg_width(bl.kind, small, true) + 16.;
    let tag_h = small * LINE + 6.;
    let mut d = PathData::new();
    let (tx, ty) = (x0 + 0.5, bl.top + 0.5);
    d.move_to(tx + 6., ty);
    d.line_to(tx + tag_w, ty);
    d.line_to(tx + tag_w, ty + tag_h - 6.);
    d.line_to(tx + tag_w - 6., ty + tag_h);
    d.line_to(tx, ty + tag_h);
    d.line_to(tx, ty + 6.);
    d.d.push('Q');
    svg::num(&mut d.d, tx);
    d.d.push(' ');
    svg::num(&mut d.d, ty);
    d.d.push(' ');
    svg::num(&mut d.d, tx + 6.);
    d.d.push(' ');
    svg::num(&mut d.d, ty);
    d.close();
    path(o, &d.d, &Style { fill: Some(th.tint), stroke: Some(th.node_stroke), stroke_width: 1., opacity: 1., ..Default::default() });
    text_line(o, tx + tag_w / 2., ty + tag_h / 2., bl.kind, small, th.text, 600, Anchor::Middle);
    for (i, l) in bl.label.iter().enumerate() {
        text_line(o, tx + tag_w + 10., ty + tag_h / 2. + i as f64 * small * LINE, &format!("[{l}]"), small, th.muted, 400, Anchor::Start);
    }
    let sep = Style { stroke: Some(th.node_stroke), stroke_width: 1., dash: Some("4,3".into()), opacity: 1., ..Default::default() };
    for (sy, label) in &bl.seps {
        let mut d = PathData::new();
        d.move_to(x0 + 0.5, sy + 0.5);
        d.line_to(x1 - 0.5, sy + 0.5);
        path(o, &d.d, &sep);
        for (i, l) in label.iter().enumerate() {
            text_line(o, (x0 + x1) / 2., sy + 4. + small * LINE * (i as f64 + 0.5), &format!("[{l}]"), small, th.muted, 400, Anchor::Middle);
        }
    }
}

fn participant(o: &mut String, p: &Participant, y: f64, th: &Theme) {
    if p.actor {
        let st = Style { fill: Some(th.node_fill), stroke: Some(th.node_stroke), stroke_width: 1.25, opacity: 1., round_joins: true, ..Default::default() };
        let cx = p.x;
        let mut d = PathData::new();
        d.move_to(cx, y + 13.);
        d.line_to(cx, y + 23.);
        d.move_to(cx - 9., y + 16.);
        d.line_to(cx + 9., y + 16.);
        d.move_to(cx - 8., y + 33.);
        d.line_to(cx, y + 23.);
        d.line_to(cx + 8., y + 33.);
        path(o, &d.d, &Style { fill: None, ..st.clone() });
        o.push_str("<circle cx=\"");
        svg::num(o, cx);
        o.push_str("\" cy=\"");
        svg::num(o, y + 7.);
        o.push_str("\" r=\"6\"");
        st.write(o);
        o.push_str("/>\n");
        let (_, th_) = measure(&p.label, th.font, false);
        text_block(o, cx, y + 38. + th_ / 2., &p.label, th.font, th.text, 400, Anchor::Middle);
    } else {
        let st = Style { fill: Some(th.node_fill), stroke: Some(th.node_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
        rect(o, p.x - p.w / 2. + 0.5, y + 0.5, p.w - 1., p.h - 1., 6., &st);
        text_block(o, p.x, y + p.h / 2., &p.label, th.font, th.text, 500, Anchor::Middle);
    }
}

fn tip(o: &mut String, t: Tip, at: (f64, f64), dir: (f64, f64), th: &Theme) {
    let (l, hw) = (8., 4.);
    let base = (at.0 - dir.0 * l, at.1 - dir.1 * l);
    let perp = (-dir.1, dir.0);
    let a = (base.0 + perp.0 * hw, base.1 + perp.1 * hw);
    let b = (base.0 - perp.0 * hw, base.1 - perp.1 * hw);
    match t {
        Tip::None => {}
        Tip::Filled => {
            let mut d = PathData::new();
            d.move_to(at.0, at.1);
            d.line_to(a.0, a.1);
            d.line_to(b.0, b.1);
            d.close();
            path(o, &d.d, &Style { fill: Some(th.edge), stroke: Some(th.edge), stroke_width: 1., opacity: 1., round_joins: true, ..Default::default() });
        }
        Tip::Open => {
            let mut d = PathData::new();
            d.move_to(a.0, a.1);
            d.line_to(at.0, at.1);
            d.line_to(b.0, b.1);
            path(o, &d.d, &Style { stroke: Some(th.edge), stroke_width: 1.25, opacity: 1., round_joins: true, ..Default::default() });
        }
        Tip::Cross => {
            let c = (at.0 - dir.0 * 5., at.1 - dir.1 * 5.);
            let s = 4.;
            let mut d = PathData::new();
            d.move_to(c.0 - s, c.1 - s);
            d.line_to(c.0 + s, c.1 + s);
            d.move_to(c.0 - s, c.1 + s);
            d.line_to(c.0 + s, c.1 - s);
            path(o, &d.d, &Style { stroke: Some(th.edge), stroke_width: 1.5, opacity: 1., round_joins: true, ..Default::default() });
        }
    }
}

/// An `autonumber` badge.
fn number(o: &mut String, at: (f64, f64), n: u32, th: &Theme) {
    let s = n.to_string();
    let r = (crate::text::width(&s, 10.5, true) / 2. + 4.).max(8.);
    o.push_str("<circle cx=\"");
    svg::num(o, at.0);
    o.push_str("\" cy=\"");
    svg::num(o, at.1);
    o.push_str("\" r=\"");
    svg::num(o, r);
    o.push('"');
    Style::fill(th.muted).write(o);
    o.push_str("/>\n");
    text_line(o, at.0, at.1, &s, 10.5, th.bg, 600, Anchor::Middle);
}

fn rect(o: &mut String, x: f64, y: f64, w: f64, h: f64, r: f64, style: &Style) {
    o.push_str("<rect x=\"");
    svg::num(o, x);
    o.push_str("\" y=\"");
    svg::num(o, y);
    o.push_str("\" width=\"");
    svg::num(o, w.max(0.));
    o.push_str("\" height=\"");
    svg::num(o, h.max(0.));
    o.push('"');
    if r > 0. {
        o.push_str(" rx=\"");
        svg::num(o, r);
        o.push('"');
    }
    style.write(o);
    o.push_str("/>\n");
}

fn path(o: &mut String, d: &str, style: &Style) {
    o.push_str("<path d=\"");
    o.push_str(d);
    o.push('"');
    style.write(o);
    o.push_str("/>\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Parsed {
        let th = Theme::new(false);
        let mut p = Parsed::default();
        for l in src.lines().skip(1) {
            let l = l.trim();
            if !l.is_empty() {
                p.line(l, &th).unwrap_or_else(|| panic!("failed on {l:?}"));
            }
        }
        p
    }

    #[test]
    fn messages_and_participants() {
        let p = parse(
            "sequenceDiagram\nparticipant A as Alice\nactor B as Bob\nA->>B: hi\nB-->>A: yo\nA->B: x\nA-->B: x\nA-xB: x\nA--xB: x\nA-)B: x\nA--)B: x\nA<<->>B: x\nA->>+C: open\nC-->>-A: close\nJohn Doe->>A: spaces",
        );
        assert_eq!(p.parts.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["A", "B", "C", "John Doe"]);
        assert_eq!(p.parts[0].label, vec!["Alice"]);
        assert!(p.parts[1].actor);
        let arrows: Vec<Arrow> = p
            .events
            .iter()
            .filter_map(|e| match e {
                Ev::Msg { arrow, .. } => Some(*arrow),
                _ => None,
            })
            .collect();
        assert_eq!(arrows[0], Arrow { dotted: false, tip: Tip::Filled, both: false });
        assert_eq!(arrows[1], Arrow { dotted: true, tip: Tip::Filled, both: false });
        assert_eq!(arrows[2].tip, Tip::None);
        assert!(arrows[3].dotted && arrows[3].tip == Tip::None);
        assert_eq!((arrows[4].tip, arrows[5].dotted), (Tip::Cross, true));
        assert_eq!(arrows[6].tip, Tip::Open);
        assert!(arrows[8].both);
        assert!(matches!(p.events[9], Ev::Msg { activate: true, to: 2, .. }));
        assert!(matches!(p.events[10], Ev::Msg { deactivate: true, from: 2, .. }));
    }

    #[test]
    fn blocks_notes_and_numbers() {
        let p = parse("sequenceDiagram\nautonumber\nloop Every minute\nA->>B: ping\nend\nalt ok\nB->>A: pong\nelse failed\nNote over A,B: retry\nend\nNote left of A: l\nnote right of B: r\nrect rgba(0, 0, 255, .1)\nA->>A: self\nend\nbox Aqua Team\nparticipant C\nend\ntitle: Hello");
        assert!(matches!(&p.events[0], Ev::Start { kind: "loop", .. }));
        assert!(p.events.iter().any(|e| matches!(e, Ev::Sep { .. })));
        assert!(p.events.iter().any(|e| matches!(e, Ev::Note { pos: NotePos::Over, a: 0, b: 1, .. })));
        assert!(matches!(&p.events[1], Ev::Msg { num: Some(1), .. }));
        assert_eq!(p.groups[0].label, vec!["Team"]);
        assert_eq!(p.groups[0].members, vec![2]);
        assert_eq!(p.title.as_deref(), Some("Hello"));
        assert!(p.stack.is_empty());
    }

    #[test]
    fn rejects_unknown_lines() {
        let th = Theme::new(false);
        for l in ["A->>B", "Note somewhere: x", "else", "create participant X", "what is this"] {
            let mut p = Parsed::default();
            // `A->>B` without text is fine; the rest must fail.
            let ok = p.line(l, &th).is_some();
            assert_eq!(ok, l == "A->>B", "{l}");
        }
    }
}
