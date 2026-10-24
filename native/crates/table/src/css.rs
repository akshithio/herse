//! Just enough CSS to resolve pandas Styler output: rules with type / `#id` /
//! `.class` / `*` compounds, descendant and child combinators, structural
//! pseudo-classes (`:nth-child(odd)`, `:first-child`, ...), `!important`, and
//! specificity + source order. Only the properties the native grid can draw
//! are kept: `background(-color)`, `color`, `font-weight`, `font-style`,
//! `font`, `text-align` and `white-space`. Everything else (`@media`, hover,
//! siblings, attribute selectors, borders, widths) is ignored.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::{Align, DataBar, Rgba};

#[path = "named_colors.rs"]
mod named_colors;
use named_colors::NAMED_COLORS;

/// FxHash: Styler sheets are thousands of short `#T_..._rowN_colM` keys.
#[derive(Default)]
pub(crate) struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let mut h = self.0;
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(*c)).wrapping_mul(K);
        }
        for &b in rest {
            h = (h.rotate_left(5) ^ b as u64).wrapping_mul(K);
        }
        self.0 = h;
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Decl {
    /// `background-color` (`None` = transparent).
    BgColor(Option<Rgba>),
    /// `background` shorthand: resets the color and sets a data bar from a
    /// horizontal `linear-gradient`.
    Background(Option<Rgba>, Option<DataBar>),
    Color(Rgba),
    Bold(bool),
    Italic(bool),
    Align(Align),
    Pre(bool),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DeclEntry {
    pub(crate) decl: Decl,
    pub(crate) important: bool,
}

/// One cascaded property: the winning priority key and its value.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Slot<T> {
    key: u64,
    pub(crate) val: Option<T>,
}

impl<T: Copy> Slot<T> {
    #[inline]
    fn set(&mut self, key: u64, v: T) {
        if self.val.is_none() || key >= self.key {
            self.key = key;
            self.val = Some(v);
        }
    }
}

/// The properties an element ends up with after the cascade (not yet
/// inherited from ancestors).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Computed {
    pub(crate) bg: Slot<Option<Rgba>>,
    pub(crate) bar: Slot<Option<DataBar>>,
    pub(crate) fg: Slot<Rgba>,
    pub(crate) bold: Slot<bool>,
    pub(crate) italic: Slot<bool>,
    pub(crate) align: Slot<Align>,
    pub(crate) pre: Slot<bool>,
}

impl Computed {
    fn apply(&mut self, d: &DeclEntry, key: u64, allow_align: bool) {
        let key = key | ((d.important as u64) << 63);
        match d.decl {
            Decl::BgColor(c) => self.bg.set(key, c),
            Decl::Background(c, bar) => {
                self.bg.set(key, c);
                self.bar.set(key, bar);
            }
            Decl::Color(c) => self.fg.set(key, c),
            Decl::Bold(b) => self.bold.set(key, b),
            Decl::Italic(b) => self.italic.set(key, b),
            Decl::Align(a) => {
                if allow_align {
                    self.align.set(key, a)
                }
            }
            Decl::Pre(p) => self.pre.set(key, p),
        }
    }
}

/// What a selector can test about an element.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Elem<'a> {
    /// Lowercase for table elements; as written for the wrappers around it.
    pub(crate) tag: &'a str,
    pub(crate) id: &'a str,
    pub(crate) class: &'a str,
    /// 1-based position among element siblings; 0 = unknown.
    pub(crate) index: u32,
    pub(crate) index_of_type: u32,
    /// Sibling counts (including itself); 0 = unknown.
    pub(crate) count: u32,
    pub(crate) count_of_type: u32,
}

impl Elem<'_> {
    fn has_class(&self, c: &str) -> bool {
        self.class.split_ascii_whitespace().any(|x| x == c)
    }
}

#[derive(Clone, Copy, Debug)]
enum Pseudo {
    /// `an+b`, optionally counting from the end and / or only same-type siblings.
    Nth { a: i32, b: i32, of_type: bool, from_end: bool },
}

#[derive(Debug, Default)]
struct Compound {
    tag: Option<Box<str>>,
    id: Option<Box<str>>,
    classes: Vec<Box<str>>,
    pseudos: Vec<Pseudo>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Comb {
    Descendant,
    Child,
}

#[derive(Debug)]
struct Selector {
    /// The rightmost compound. When the rule is indexed by the subject's id,
    /// that id moves into the index (the bucket lookup already checked it).
    subject: Compound,
    /// The ancestors, right to left, each with the combinator joining it to
    /// the compound on its right.
    ancestors: Vec<(Comb, Compound)>,
    specificity: u32,
    /// pandas / polars boilerplate (`.dataframe thead th { text-align: right }`):
    /// its `text-align` is ignored so the grid can align by content instead.
    boilerplate: bool,
}

#[derive(Debug)]
struct Rule {
    sel: Selector,
    decls: (u32, u32),
    order: u32,
    /// Next rule in the same index bucket (`NONE` ends the list).
    next: u32,
}

const NONE: u32 = u32::MAX;

pub(crate) struct StyleSheet {
    rules: Vec<Rule>,
    decls: Vec<DeclEntry>,
    /// Index buckets: key -> first rule of a linked list through `Rule::next`.
    by_id: FxMap<Box<str>, u32>,
    by_class: FxMap<Box<str>, u32>,
    by_tag: FxMap<Box<str>, u32>,
    universal: u32,
    /// Some rule sets `white-space`: rows need a cascade lookup while their
    /// cells' text is collected.
    pub(crate) has_white_space: bool,
    /// ... and some such rule can target a cell itself (`td`, `th`, `*`,
    /// `.class`, `#id` subjects), so each cell needs one too.
    pub(crate) ws_cells: bool,
    /// Some selector uses `:nth-child` & co: siblings are not interchangeable.
    pub(crate) structural: bool,
}

const INLINE_SPECIFICITY: u64 = 1 << 30;

impl Default for StyleSheet {
    fn default() -> Self {
        StyleSheet {
            rules: Vec::new(),
            decls: Vec::new(),
            by_id: FxMap::default(),
            by_class: FxMap::default(),
            by_tag: FxMap::default(),
            universal: NONE,
            has_white_space: false,
            ws_cells: false,
            structural: false,
        }
    }
}

impl StyleSheet {
    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Parse a `<style>` block and add its rules.
    pub(crate) fn add(&mut self, css: &str) {
        let css = strip_comments(css);
        let b = css.as_bytes();
        let mut i = 0;
        while i < b.len() {
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b';' || b[i] == b'}') {
                i += 1;
            }
            if i >= b.len() {
                break;
            }
            if b[i] == b'@' {
                // Skip the at-rule: up to `;` or its whole `{ ... }` block.
                let mut j = i;
                while j < b.len() && b[j] != b'{' && b[j] != b';' {
                    j += 1;
                }
                i = if j < b.len() && b[j] == b'{' { skip_block(b, j) } else { j + 1 };
                continue;
            }
            let pre_start = i;
            while i < b.len() && b[i] != b'{' {
                i += 1;
            }
            if i >= b.len() {
                break;
            }
            let prelude = css.get(pre_start..i).unwrap_or("");
            let body_start = i + 1;
            let end = skip_block(b, i);
            let body_end = if end > body_start && b.get(end - 1) == Some(&b'}') { end - 1 } else { end };
            let body = css.get(body_start..body_end).unwrap_or("");
            i = end;
            let d0 = self.decls.len();
            parse_declarations(body, &mut self.decls);
            let d1 = self.decls.len();
            if d0 == d1 {
                continue;
            }
            let sets_ws = self.decls[d0..d1].iter().any(|d| matches!(d.decl, Decl::Pre(_)));
            let mut added = false;
            for sel_text in split_top_level(prelude, b',') {
                let Some(mut sel) = parse_selector(sel_text) else { continue };
                if !sel.subject.pseudos.is_empty() || sel.ancestors.iter().any(|(_, p)| !p.pseudos.is_empty()) {
                    self.structural = true;
                }
                if sets_ws {
                    self.has_white_space = true;
                    if sel.subject.tag.as_deref().is_none_or(|t| t == "td" || t == "th") {
                        self.ws_cells = true;
                    }
                }
                let idx = self.rules.len() as u32;
                let subject = &mut sel.subject;
                let head = if let Some(id) = subject.id.take() {
                    self.by_id.entry(id).or_insert(NONE)
                } else if let Some(c) = subject.classes.first() {
                    self.by_class.entry(c.clone()).or_insert(NONE)
                } else if let Some(t) = &subject.tag {
                    self.by_tag.entry(t.clone()).or_insert(NONE)
                } else {
                    &mut self.universal
                };
                let next = std::mem::replace(head, idx);
                self.rules.push(Rule { sel, decls: (d0 as u32, d1 as u32), order: idx, next });
                added = true;
            }
            if !added {
                self.decls.truncate(d0);
            }
        }
    }

    /// Whether any rule could match an element with this tag / id / class
    /// (cheap pre-check before building a selector chain).
    pub(crate) fn may_match(&self, tag: &str, id: &str, class: &str) -> bool {
        self.universal != NONE
            || self.by_tag.contains_key(tag)
            || (!id.is_empty() && self.by_id.contains_key(id))
            || (!class.is_empty() && class.split_ascii_whitespace().any(|c| self.by_class.contains_key(c)))
    }

    /// Cascade the rules (and an inline `style` attribute) for the last
    /// element of `chain`.
    pub(crate) fn compute(&self, chain: &[Elem], inline: &str, out: &mut Computed) {
        let Some(el) = chain.last() else { return };
        if !self.rules.is_empty() {
            let mut visit = |mut r: u32| {
                while let Some(rule) = self.rules.get(r as usize) {
                    if matches(&rule.sel, chain) {
                        let key = ((rule.sel.specificity as u64) << 32) | rule.order as u64;
                        for d in &self.decls[rule.decls.0 as usize..rule.decls.1 as usize] {
                            out.apply(d, key, !rule.sel.boilerplate);
                        }
                    }
                    r = rule.next;
                }
            };
            if !el.id.is_empty()
                && let Some(&l) = self.by_id.get(el.id)
            {
                visit(l);
            }
            if !self.by_class.is_empty() {
                for c in el.class.split_ascii_whitespace() {
                    if let Some(&l) = self.by_class.get(c) {
                        visit(l);
                    }
                }
            }
            if let Some(&l) = self.by_tag.get(el.tag) {
                visit(l);
            }
            visit(self.universal);
        }
        if !inline.is_empty() {
            let mut tmp = Vec::new();
            parse_declarations(inline, &mut tmp);
            for (i, d) in tmp.iter().enumerate() {
                out.apply(d, (INLINE_SPECIFICITY << 32) | i as u64, true);
            }
        }
    }
}

fn strip_comments(css: &str) -> std::borrow::Cow<'_, str> {
    if !css.contains("/*") {
        return css.into();
    }
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        out.push(' ');
        rest = match rest[i + 2..].find("*/") {
            Some(j) => &rest[i + 2 + j + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out.into()
}

/// `b[open] == b'{'`; returns the index just past the matching `}`.
fn skip_block(b: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    let mut quote = 0u8;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            } else if c == b'\\' {
                i += 1;
            }
        } else {
            match c {
                b'"' | b'\'' => quote = c,
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    b.len()
}

/// Split on `sep` outside parentheses, brackets and quotes.
fn split_top_level(s: &str, sep: u8) -> SplitTop<'_> {
    SplitTop { s, pos: 0, sep, done: false }
}

struct SplitTop<'a> {
    s: &'a str,
    pos: usize,
    sep: u8,
    done: bool,
}

impl<'a> Iterator for SplitTop<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.done {
            return None;
        }
        let b = self.s.as_bytes();
        let (mut depth, mut quote) = (0i32, 0u8);
        let start = self.pos;
        let mut i = start;
        while i < b.len() {
            let c = b[i];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else {
                match c {
                    b'"' | b'\'' => quote = c,
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    _ if c == self.sep && depth <= 0 => {
                        self.pos = i + 1;
                        return Some(self.s.get(start..i).unwrap_or(""));
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        self.done = true;
        Some(self.s.get(start..).unwrap_or(""))
    }
}

/// Parse `prop: value; ...`, keeping only the properties we can draw.
pub(crate) fn parse_declarations(body: &str, out: &mut Vec<DeclEntry>) {
    for decl in split_top_level(body, b';') {
        let Some((prop, value)) = decl.split_once(':') else { continue };
        let prop = prop.trim();
        let mut value = value.trim();
        let mut important = false;
        if let Some(i) = value.rfind('!')
            && value[i + 1..].trim().eq_ignore_ascii_case("important")
        {
            important = true;
            value = value[..i].trim_end();
        }
        let mut push = |decl| out.push(DeclEntry { decl, important });
        let p = |name: &str| prop.eq_ignore_ascii_case(name);
        if p("background-color") {
            if let Some(c) = parse_color(value) {
                push(Decl::BgColor(c));
            }
        } else if p("background") {
            if let Some((c, bar)) = parse_background(value) {
                push(Decl::Background(c, bar));
            }
        } else if p("color") {
            if let Some(Some(c)) = parse_color(value) {
                push(Decl::Color(c));
            }
        } else if p("font-weight") {
            if let Some(b) = parse_weight(value) {
                push(Decl::Bold(b));
            }
        } else if p("font-style") {
            let v = value.to_ascii_lowercase();
            match v.as_str() {
                "italic" | "oblique" => push(Decl::Italic(true)),
                "normal" => push(Decl::Italic(false)),
                _ if v.starts_with("oblique") => push(Decl::Italic(true)),
                _ => {}
            }
        } else if p("font") {
            // Shorthand: resets weight and style, then sets what it names.
            let mut bold = false;
            let mut italic = false;
            for tok in value.split_ascii_whitespace() {
                if tok.eq_ignore_ascii_case("italic") || tok.eq_ignore_ascii_case("oblique") {
                    italic = true;
                } else if let Some(true) = parse_weight(tok) {
                    bold = true;
                }
            }
            if !value.is_empty() && !is_global_keyword(value) {
                push(Decl::Bold(bold));
                push(Decl::Italic(italic));
            }
        } else if p("text-align") {
            let a = match value.to_ascii_lowercase().as_str() {
                "left" | "start" | "justify" | "-webkit-left" => Some(Align::Left),
                "right" | "end" | "-webkit-right" => Some(Align::Right),
                "center" | "-webkit-center" | "-moz-center" => Some(Align::Center),
                _ => None,
            };
            if let Some(a) = a {
                push(Decl::Align(a));
            }
        } else if p("white-space") {
            let v = value.to_ascii_lowercase();
            match v.as_str() {
                "pre" | "pre-wrap" | "break-spaces" => push(Decl::Pre(true)),
                "normal" | "nowrap" | "pre-line" => push(Decl::Pre(false)),
                _ => {}
            }
        }
    }
}

fn is_global_keyword(v: &str) -> bool {
    ["inherit", "initial", "unset", "revert", "revert-layer"].iter().any(|k| v.eq_ignore_ascii_case(k))
}

fn parse_weight(v: &str) -> Option<bool> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("bold") || v.eq_ignore_ascii_case("bolder") {
        Some(true)
    } else if v.eq_ignore_ascii_case("normal") || v.eq_ignore_ascii_case("lighter") {
        Some(false)
    } else {
        v.parse::<f32>().ok().filter(|w| (1.0..=1000.0).contains(w)).map(|w| w >= 600.0)
    }
}

/// `background` shorthand: the color layer and a pandas `bar()` gradient.
/// `None` for values we cannot interpret (keyword `inherit`, ...).
fn parse_background(v: &str) -> Option<(Option<Rgba>, Option<DataBar>)> {
    if is_global_keyword(v) {
        return None;
    }
    let mut color = None;
    let mut bar = None;
    for tok in split_ws_top_level(v) {
        let lower_prefix = tok.get(..16).unwrap_or(tok).to_ascii_lowercase();
        if lower_prefix.starts_with("linear-gradient(") {
            if bar.is_none() {
                bar = parse_bar(tok);
            }
        } else if let Some(c) = parse_color(tok) {
            color = c;
        }
    }
    Some((color, bar))
}

fn split_ws_top_level(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let (mut depth, mut start) = (0i32, None::<usize>);
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        let sep = (c.is_ascii_whitespace() || c == b',') && depth <= 0;
        match (sep, start) {
            (true, Some(s0)) => {
                out.push(&s[s0..i]);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s0) = start {
        out.push(&s[s0..]);
    }
    out
}

/// A horizontal hard-stop gradient as written by pandas `Styler.bar()`:
/// `linear-gradient(90deg, transparent 20%, #86efac 20%, #86efac 53%, transparent 53%)`.
fn parse_bar(tok: &str) -> Option<DataBar> {
    let open = tok.find('(')?;
    let inner = tok[open + 1..].strip_suffix(')').unwrap_or(&tok[open + 1..]);
    let mut args: Vec<&str> = split_top_level(inner, b',').map(str::trim).collect();
    let first = args.first()?.to_ascii_lowercase();
    if first.ends_with("deg") || first.starts_with("to ") || first.ends_with("turn") || first.ends_with("rad") {
        let horizontal = first == "90deg" || first == "to right" || first == "0.25turn";
        if !horizontal {
            return None;
        }
        args.remove(0);
    } else {
        // Default direction is top-to-bottom: not a bar.
        return None;
    }
    // (color, position%) stops; missing positions are spread evenly.
    let mut stops: Vec<(Option<Rgba>, Option<f32>)> = Vec::with_capacity(args.len());
    for a in &args {
        let parts = split_ws_top_level(a);
        let (&c, rest) = parts.split_first()?;
        let color = parse_color(c)?;
        let mut positions = rest.iter().filter_map(|p| p.strip_suffix('%')?.trim().parse::<f32>().ok());
        let p1 = positions.next();
        stops.push((color, p1));
        if let Some(p2) = positions.next() {
            stops.push((color, Some(p2)));
        }
    }
    if stops.len() < 2 {
        return None;
    }
    let n = stops.len();
    if stops[0].1.is_none() {
        stops[0].1 = Some(0.0);
    }
    if stops[n - 1].1.is_none() {
        stops[n - 1].1 = Some(100.0);
    }
    // Fill gaps by interpolation; positions never go backwards.
    let mut i = 0;
    while i < n {
        if stops[i].1.is_none() {
            let prev = i - 1;
            let mut next = i;
            while stops[next].1.is_none() {
                next += 1;
            }
            let (p0, p1) = (stops[prev].1.unwrap_or(0.0), stops[next].1.unwrap_or(100.0));
            for (k, s) in stops.iter_mut().enumerate().take(next).skip(i) {
                s.1 = Some(p0 + (p1 - p0) * (k - prev) as f32 / (next - prev) as f32);
            }
            i = next;
        }
        i += 1;
    }
    let mut last = 0.0f32;
    let pos: Vec<f32> = stops
        .iter()
        .map(|s| {
            last = s.1.unwrap_or(last).max(last);
            last
        })
        .collect();
    // Colored intervals: before the first stop, between stops, after the last.
    let mut best: Option<(f32, f32, Rgba)> = None;
    let mut extent: Option<(f32, f32)> = None;
    let mut consider = |a: f32, b: f32, c: Option<Rgba>| {
        if let Some(c) = c.filter(|c| c.a > 0)
            && b > a
        {
            extent = Some(extent.map_or((a, b), |(x, y)| (x.min(a), y.max(b))));
            if best.is_none_or(|(x, y, _)| b - a > y - x) {
                best = Some((a, b, c));
            }
        }
    };
    consider(0.0, pos[0], stops[0].0);
    for k in 0..n - 1 {
        let c = if stops[k].0 == stops[k + 1].0 { stops[k].0 } else { stops[k].0.or(stops[k + 1].0) };
        consider(pos[k], pos[k + 1], c);
    }
    consider(pos[n - 1], 100.0, stops[n - 1].0);
    let (_, _, color) = best?;
    let (a, b) = extent?;
    Some(DataBar { start: (a / 100.0).clamp(0.0, 1.0), end: (b / 100.0).clamp(0.0, 1.0), color })
}

/// A CSS color: `Some(Some(c))`, `Some(None)` for `transparent`, `None` if
/// unparseable (or `inherit` / `currentcolor`, which we do not model).
pub(crate) fn parse_color(v: &str) -> Option<Option<Rgba>> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex(hex).map(Some);
    }
    let lower;
    let v = if v.bytes().any(|b| b.is_ascii_uppercase()) {
        lower = v.to_ascii_lowercase();
        lower.as_str()
    } else {
        v
    };
    if v == "transparent" {
        return Some(None);
    }
    if let Some(open) = v.find('(') {
        let name = v[..open].trim();
        let args = v[open + 1..].trim_end().strip_suffix(')').unwrap_or(&v[open + 1..]);
        return match name {
            "rgb" | "rgba" => parse_rgb_args(args).map(Some),
            "hsl" | "hsla" => parse_hsl_args(args).map(Some),
            _ => None,
        };
    }
    NAMED_COLORS
        .binary_search_by(|(k, _)| k.cmp(&v))
        .ok()
        .map(|i| Some(Rgba::from_rgb_u32(NAMED_COLORS[i].1)))
}

fn parse_hex(h: &str) -> Option<Rgba> {
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let d = |i: usize| u8::from_str_radix(h.get(i..i + 1)?, 16).ok();
    let dd = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    Some(match h.len() {
        3 => Rgba { r: d(0)? * 17, g: d(1)? * 17, b: d(2)? * 17, a: 255 },
        4 => Rgba { r: d(0)? * 17, g: d(1)? * 17, b: d(2)? * 17, a: d(3)? * 17 },
        6 => Rgba { r: dd(0)?, g: dd(2)?, b: dd(4)?, a: 255 },
        8 => Rgba { r: dd(0)?, g: dd(2)?, b: dd(4)?, a: dd(6)? },
        _ => return None,
    })
}

/// Components of `rgb()` / `hsl()`: comma or space separated, optional `/ alpha`.
fn color_args(args: &str) -> Vec<&str> {
    args.split([',', ' ', '\t', '\n', '/']).filter(|s| !s.is_empty()).collect()
}

fn num(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

fn alpha(s: Option<&&str>) -> Option<u8> {
    let Some(s) = s else { return Some(255) };
    let v = match s.strip_suffix('%') {
        Some(p) => num(p)? / 100.0,
        None => num(s)?,
    };
    Some((v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn parse_rgb_args(args: &str) -> Option<Rgba> {
    let a = color_args(args);
    if a.len() < 3 {
        return None;
    }
    let ch = |s: &str| -> Option<u8> {
        let v = match s.strip_suffix('%') {
            Some(p) => num(p)? * 2.55,
            None => num(s)?,
        };
        Some(v.clamp(0.0, 255.0).round() as u8)
    };
    Some(Rgba { r: ch(a[0])?, g: ch(a[1])?, b: ch(a[2])?, a: alpha(a.get(3))? })
}

fn parse_hsl_args(args: &str) -> Option<Rgba> {
    let a = color_args(args);
    if a.len() < 3 {
        return None;
    }
    let h = a[0];
    let hue = if let Some(d) = h.strip_suffix("deg") {
        num(d)?
    } else if let Some(t) = h.strip_suffix("turn") {
        num(t)? * 360.0
    } else if let Some(r) = h.strip_suffix("rad") {
        num(r)?.to_degrees()
    } else {
        num(h)?
    };
    let pct = |s: &str| Some((num(s.strip_suffix('%').unwrap_or(s))? / 100.0).clamp(0.0, 1.0));
    let (s, l) = (pct(a[1])?, pct(a[2])?);
    let hue = hue.rem_euclid(360.0) / 360.0;
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Some(Rgba { r: f(hue + 1.0 / 3.0), g: f(hue), b: f(hue - 1.0 / 3.0), a: alpha(a.get(3))? })
}

fn parse_selector(text: &str) -> Option<Selector> {
    let b = text.trim().as_bytes();
    let text = text.trim();
    if b.is_empty() {
        return None;
    }
    let mut parts: Vec<Compound> = Vec::new();
    let mut combs: Vec<Comb> = Vec::new();
    let mut cur = Compound::default();
    let mut cur_empty = true;
    let mut pending: Option<Comb> = None;
    let mut spec = (0u32, 0u32, 0u32);
    let mut i = 0;
    fn ident<'t>(text: &'t str, i: &mut usize) -> &'t str {
        let b = text.as_bytes();
        let s = *i;
        while *i < b.len() && (b[*i].is_ascii_alphanumeric() || matches!(b[*i], b'-' | b'_' | b'\\') || b[*i] >= 0x80)
        {
            if b[*i] == b'\\' {
                *i += 1;
            }
            *i += 1;
        }
        *i = (*i).min(b.len());
        // Never split a UTF-8 sequence (an escape before a multi-byte char).
        while *i < b.len() && !text.is_char_boundary(*i) {
            *i += 1;
        }
        text.get(s..*i).unwrap_or("")
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() || c == b'>' || c == b'+' || c == b'~' {
            let mut comb = Comb::Descendant;
            while i < b.len() && (b[i].is_ascii_whitespace() || matches!(b[i], b'>' | b'+' | b'~')) {
                match b[i] {
                    b'>' => comb = Comb::Child,
                    b'+' | b'~' => return None, // sibling combinators: unsupported
                    _ => {}
                }
                i += 1;
            }
            if !cur_empty {
                parts.push(std::mem::take(&mut cur));
                cur_empty = true;
                pending = Some(comb);
            }
            continue;
        }
        if cur_empty {
            if let Some(p) = pending.take() {
                combs.push(p);
            }
            cur_empty = false;
        }
        match c {
            b'*' => i += 1,
            b'#' => {
                i += 1;
                let id = ident(text, &mut i);
                if id.is_empty() {
                    return None;
                }
                cur.id = Some(unescape(id));
                spec.0 += 1;
            }
            b'.' => {
                i += 1;
                let cl = ident(text, &mut i);
                if cl.is_empty() {
                    return None;
                }
                cur.classes.push(unescape(cl));
                spec.1 += 1;
            }
            b'[' => return None,
            b':' => {
                if b.get(i + 1) == Some(&b':') {
                    return None; // pseudo-elements never match a cell
                }
                i += 1;
                let name = ident(text, &mut i).to_ascii_lowercase();
                let mut arg = "";
                if b.get(i) == Some(&b'(') {
                    let s = i + 1;
                    let mut depth = 1;
                    i += 1;
                    while i < b.len() && depth > 0 {
                        match b[i] {
                            b'(' => depth += 1,
                            b')' => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                    arg = text.get(s..i.saturating_sub(1)).unwrap_or("");
                }
                let nth = |of_type, from_end| -> Option<Pseudo> {
                    let (a, b) = parse_nth(arg)?;
                    Some(Pseudo::Nth { a, b, of_type, from_end })
                };
                let first = |of_type, from_end| Pseudo::Nth { a: 0, b: 1, of_type, from_end };
                match name.as_str() {
                    "first-child" => cur.pseudos.push(first(false, false)),
                    "last-child" => cur.pseudos.push(first(false, true)),
                    "only-child" => cur.pseudos.extend([first(false, false), first(false, true)]),
                    "first-of-type" => cur.pseudos.push(first(true, false)),
                    "last-of-type" => cur.pseudos.push(first(true, true)),
                    "only-of-type" => cur.pseudos.extend([first(true, false), first(true, true)]),
                    "nth-child" => cur.pseudos.push(nth(false, false)?),
                    "nth-last-child" => cur.pseudos.push(nth(false, true)?),
                    "nth-of-type" => cur.pseudos.push(nth(true, false)?),
                    "nth-last-of-type" => cur.pseudos.push(nth(true, true)?),
                    // hover, focus, not(), is(), ...: never matches a static grid.
                    _ => return None,
                }
                spec.1 += 1;
            }
            _ => {
                let t = ident(text, &mut i);
                if t.is_empty() {
                    return None;
                }
                cur.tag = Some(t.to_ascii_lowercase().into());
                spec.2 += 1;
            }
        }
    }
    if !cur_empty {
        parts.push(cur);
    }
    if parts.is_empty() || combs.len() + 1 != parts.len() {
        return None;
    }
    let specificity = (spec.0.min(1023) << 20) | (spec.1.min(1023) << 10) | spec.2.min(1023);
    let boilerplate = parts.iter().any(|p| p.classes.iter().any(|c| &**c == "dataframe" || &**c == "data-frame"));
    let subject = parts.pop()?;
    // `combs[i]` joins `parts[i]` to `parts[i + 1]`; walk right to left.
    let ancestors = parts.into_iter().zip(combs).rev().map(|(p, c)| (c, p)).collect();
    Some(Selector { subject, ancestors, specificity, boilerplate })
}

fn unescape(s: &str) -> Box<str> {
    if !s.contains('\\') {
        return s.into();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out.into()
}

/// `an+b` microsyntax: `odd`, `even`, `3`, `2n+1`, `-n+3`, `n`.
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let s: String = arg.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Some(npos) = s.find('n') {
        let a = match &s[..npos] {
            "" | "+" => 1,
            "-" => -1,
            x => x.parse().ok()?,
        };
        let rest = &s[npos + 1..];
        let b = if rest.is_empty() { 0 } else { rest.strip_prefix('+').unwrap_or(rest).parse().ok()? };
        Some((a, b))
    } else {
        Some((0, s.parse().ok()?))
    }
}

fn compound_matches(c: &Compound, e: &Elem) -> bool {
    if let Some(t) = &c.tag
        && !e.tag.eq_ignore_ascii_case(t)
    {
        return false;
    }
    if let Some(id) = &c.id
        && e.id != &**id
    {
        return false;
    }
    if !c.classes.iter().all(|cl| e.has_class(cl)) {
        return false;
    }
    c.pseudos.iter().all(|p| match *p {
        Pseudo::Nth { a, b, of_type, from_end } => {
            let (idx, count) = if of_type { (e.index_of_type, e.count_of_type) } else { (e.index, e.count) };
            if idx == 0 || (from_end && count == 0) {
                return false;
            }
            let n = if from_end { count as i32 - idx as i32 + 1 } else { idx as i32 };
            if a == 0 {
                n == b
            } else {
                let d = n - b;
                d % a == 0 && d / a >= 0
            }
        }
    })
}

/// Longest element chain the matcher handles (wrappers + table + section +
/// row + cell); the builder keeps at most 256 wrappers.
const MAX_CHAIN: usize = 320;

/// Right-to-left matching over the *set* of chain positions the selector so
/// far can sit at, so `div div div ... td` against deep wrappers stays
/// O(compounds × depth) instead of backtracking exponentially. For a
/// descendant combinator only the highest candidate position matters.
fn matches(sel: &Selector, chain: &[Elem]) -> bool {
    let Some(last) = chain.len().checked_sub(1) else { return false };
    if !compound_matches(&sel.subject, &chain[last]) {
        return false;
    }
    match sel.ancestors.as_slice() {
        [] => return true,
        // The common shapes (`#T th`, `.dataframe td`, `table > tbody > tr`)
        // need no set.
        [(Comb::Child, p)] => return last > 0 && compound_matches(p, &chain[last - 1]),
        [(Comb::Descendant, p)] => return chain[..last].iter().any(|e| compound_matches(p, e)),
        _ => {}
    }
    let chain = &chain[chain.len().saturating_sub(MAX_CHAIN)..];
    let last = chain.len() - 1;
    let mut cur = [0u64; MAX_CHAIN / 64];
    let set = |b: &mut [u64; MAX_CHAIN / 64], i: usize| b[i / 64] |= 1 << (i % 64);
    let has = |b: &[u64; MAX_CHAIN / 64], i: usize| b[i / 64] & (1 << (i % 64)) != 0;
    set(&mut cur, last);
    for (comb, part) in &sel.ancestors {
        let mut next = [0u64; MAX_CHAIN / 64];
        let mut any = false;
        match comb {
            Comb::Child => {
                for i in 1..=last {
                    if has(&cur, i) && compound_matches(part, &chain[i - 1]) {
                        set(&mut next, i - 1);
                        any = true;
                    }
                }
            }
            Comb::Descendant => {
                let Some(top) = (0..=last).rev().find(|&i| has(&cur, i)) else { return false };
                for (i, e) in chain.iter().enumerate().take(top) {
                    if compound_matches(part, e) {
                        set(&mut next, i);
                        any = true;
                    }
                }
            }
        }
        if !any {
            return false;
        }
        cur = next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Option<Option<Rgba>> {
        Some(Some(Rgba { r, g, b, a: 255 }))
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#440154"), rgb(0x44, 0x01, 0x54));
        assert_eq!(parse_color("#fff"), rgb(255, 255, 255));
        assert_eq!(parse_color("#ff000080"), Some(Some(Rgba { r: 255, g: 0, b: 0, a: 128 })));
        assert_eq!(parse_color("rgb(30, 41, 59)"), rgb(30, 41, 59));
        assert_eq!(parse_color("rgba(255, 0, 0, 0.5)"), Some(Some(Rgba { r: 255, g: 0, b: 0, a: 128 })));
        assert_eq!(parse_color("rgb(100% 0% 0% / 50%)"), Some(Some(Rgba { r: 255, g: 0, b: 0, a: 128 })));
        assert_eq!(parse_color("hsl(120, 100%, 25%)"), rgb(0, 128, 0));
        assert_eq!(parse_color("Gold"), rgb(255, 215, 0));
        assert_eq!(parse_color("crimson"), rgb(220, 20, 60));
        assert_eq!(parse_color("transparent"), Some(None));
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color("notacolor"), None);
        assert_eq!(parse_color("rgb(1,2)"), None);
    }

    #[test]
    fn bars() {
        let bar = |s: &str| parse_background(s).and_then(|(_, b)| b);
        let b = bar("linear-gradient(90deg, #60a5fa 12.0%, transparent 12.0%)").unwrap();
        assert_eq!((b.start, b.end), (0.0, 0.12));
        assert_eq!(b.color, Rgba { r: 0x60, g: 0xa5, b: 0xfa, a: 255 });
        let b = bar("linear-gradient(90deg, transparent 20.0%, #86efac 20.0%, #86efac 53.3%, transparent 53.3%)")
            .unwrap();
        assert!((b.start - 0.2).abs() < 1e-6 && (b.end - 0.533).abs() < 1e-6);
        assert!(bar("linear-gradient(red, blue)").is_none());
        assert_eq!(parse_background("lightblue"), Some((Some(Rgba { r: 173, g: 216, b: 230, a: 255 }), None)));
    }

    #[test]
    fn cascade() {
        let mut ss = StyleSheet::default();
        ss.add(
            "/* c */ #T th { background-color: #111; color: white } \
             #T_r0_c1, #T_r1_c1 { background-color: #440154; color: #f1f1f1 } \
             @media (prefers-color-scheme: dark) { td { color: red } } \
             .dataframe tbody tr th { vertical-align: top } \
             .dataframe td { text-align: right } \
             tbody tr:nth-child(even) td { font-weight: bold } \
             td:hover { color: red } \
             td { color: blue !important }",
        );
        let table = Elem { tag: "table", id: "T", class: "dataframe", ..Default::default() };
        let tbody = Elem { tag: "tbody", ..Default::default() };
        let tr = |i| Elem { tag: "tr", index: i, count: 4, ..Default::default() };
        let td = Elem { tag: "td", id: "T_r0_c1", index: 2, count: 3, ..Default::default() };
        let mut c = Computed::default();
        ss.compute(&[table, tbody, tr(1), td], "", &mut c);
        assert_eq!(c.bg.val, Some(Some(Rgba { r: 0x44, g: 1, b: 0x54, a: 255 })));
        assert_eq!(c.fg.val, Some(Rgba { r: 0, g: 0, b: 255, a: 255 })); // !important wins
        assert_eq!(c.bold.val, None);
        assert_eq!(c.align.val, None); // pandas boilerplate alignment ignored
        let mut c = Computed::default();
        ss.compute(&[table, tbody, tr(2), td], "background: rgb(1,2,3); font-weight: 700", &mut c);
        assert_eq!(c.bold.val, Some(true));
        assert_eq!(c.bg.val, Some(Some(Rgba { r: 1, g: 2, b: 3, a: 255 })));
        let th = Elem { tag: "th", ..Default::default() };
        let mut c = Computed::default();
        ss.compute(&[table, tbody, tr(1), th], "", &mut c);
        assert_eq!(c.bg.val, Some(Some(Rgba { r: 0x11, g: 0x11, b: 0x11, a: 255 })));
        assert_eq!(c.fg.val, Some(Rgba { r: 255, g: 255, b: 255, a: 255 }));
    }

    #[test]
    fn nth() {
        assert_eq!(parse_nth("odd"), Some((2, 1)));
        assert_eq!(parse_nth("2n + 1"), Some((2, 1)));
        assert_eq!(parse_nth("-n+3"), Some((-1, 3)));
        assert_eq!(parse_nth("4"), Some((0, 4)));
        assert_eq!(parse_nth("x"), None);
    }
}
