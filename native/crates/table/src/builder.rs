//! Tokens -> raw rows (HTML table tree construction, simplified) -> grid.

use std::borrow::Cow;

use crate::classify::classify;
use crate::css::{self, Computed, Elem, StyleSheet};
use crate::text::{TextBuf, decode, is_html_ws};
use crate::tokenizer::{Attrs, Tag, Token, Tokenizer, find_ascii_ci};
use crate::{Align, Cell, CellKind, CellStyle, Flavor, Rgba, Table};

/// Browsers clamp colspan to 1000 and rowspan to 65534.
const MAX_COLSPAN: u16 = 1000;
const MAX_ROWSPAN: u16 = 65534;
/// Safety caps against hostile input: grid width and total grid cells.
const MAX_COLUMNS: usize = 4096;
const MAX_GRID_CELLS: usize = 4_000_000;
/// Wrappers remembered around the table (for descendant selectors).
const MAX_OUTER: usize = 256;

/// The attributes the model uses. `id` / `class` / `style` are raw slices of
/// the source (`style` is entity-decoded when used; entities in `id` /
/// `class` are not decoded, nobody writes those).
#[derive(Clone, Copy, Default)]
struct ElemAttrs<'a> {
    id: &'a str,
    class: &'a str,
    style: &'a str,
    colspan: u16,
    /// 0 = to the end of the row group.
    rowspan: u16,
    align: Option<Align>,
    bgcolor: Option<Rgba>,
    hidden: bool,
}

impl<'a> ElemAttrs<'a> {
    fn has_class(&self, c: &str) -> bool {
        !self.class.is_empty() && self.class.split_ascii_whitespace().any(|x| x == c)
    }
    fn styled(&self) -> bool {
        !self.style.is_empty() || self.bgcolor.is_some()
    }
    fn style(&self) -> Cow<'a, str> {
        decode(self.style)
    }
    fn elem(&self, tag: &'a str) -> Elem<'a> {
        Elem { tag, id: self.id, class: self.class, ..Default::default() }
    }
}

fn read_attrs(raw: &str) -> ElemAttrs<'_> {
    let mut a = ElemAttrs { colspan: 1, rowspan: 1, ..Default::default() };
    if raw.len() <= 1 {
        return a; // "" or "/"
    }
    for (name, value) in Attrs::new(raw) {
        let n = |s: &str| name.eq_ignore_ascii_case(s);
        match name.len() {
            2 if n("id") => a.id = value,
            5 if n("class") => a.class = value,
            5 if n("style") => {
                a.style = value;
                a.hidden |= value.len() >= 12 && is_display_none(&decode(value));
            }
            7 if n("colspan") => a.colspan = parse_span(value).map_or(1, |v| v.clamp(1, MAX_COLSPAN)),
            7 if n("rowspan") => a.rowspan = parse_span(value).map_or(1, |v| v.min(MAX_ROWSPAN)),
            5 | 6 if n("align") || n("halign") => {
                a.align = match value.trim().to_ascii_lowercase().as_str() {
                    "left" | "justify" => Some(Align::Left),
                    "right" => Some(Align::Right),
                    "center" | "middle" => Some(Align::Center),
                    _ => None,
                }
            }
            7 if n("bgcolor") => a.bgcolor = css::parse_color(&decode(value)).flatten(),
            6 if n("hidden") => a.hidden = true,
            _ => {}
        }
    }
    a
}

/// Leading digits, as browsers parse `colspan="2px"`.
fn parse_span(v: &str) -> Option<u16> {
    let mut n: u32 = 0;
    let mut any = false;
    for b in v.trim_start().bytes().take_while(u8::is_ascii_digit).take(6) {
        n = n * 10 + (b - b'0') as u32;
        any = true;
    }
    any.then(|| n.min(u16::MAX as u32) as u16)
}

fn is_display_none(style: &str) -> bool {
    let lower = style.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(i) = rest.find("display") {
        let after = rest[i + 7..].trim_start();
        if let Some(v) = after.strip_prefix(':')
            && v.trim_start().starts_with("none")
        {
            return true;
        }
        rest = &rest[i + 7..];
    }
    false
}

fn mentions_white_space(style: &str) -> bool {
    !style.is_empty() && memchr::memmem::find(style.as_bytes(), b"white-space").is_some()
}

fn has_non_ws(t: &str) -> bool {
    t.bytes().any(|b| !is_html_ws(b))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Before,
    In,
    After,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SecKind {
    Head,
    Body,
    Foot,
}

struct RawSection<'a> {
    kind: SecKind,
    attrs: ElemAttrs<'a>,
    tag: &'static str,
    /// Cached `white-space` inherited from the section and its ancestors.
    pre: Option<bool>,
    /// Cached `white-space` of a plain `<tr>` (no id / class / style) in it.
    plain_row_pre: Option<bool>,
}

struct RawRow<'a> {
    section: usize,
    attrs: ElemAttrs<'a>,
    cells: Vec<RawCell<'a>>,
    /// `display: none`: not drawn, but still a sibling for `:nth-child`.
    hidden: bool,
}

struct RawCell<'a> {
    text: String,
    attrs: ElemAttrs<'a>,
    /// The element was a `<th>`.
    th_tag: bool,
    /// Row label: `<th>` or Julia's `<td class="rowNumber">`.
    header: bool,
    bold_tag: bool,
    italic_tag: bool,
    style: CellStyle,
    align: Option<Align>,
}

struct Builder<'a> {
    sheet: StyleSheet,
    phase: Phase,
    standalone: bool,
    /// Google Colab's `colab-df-container`: its buttons and scripts are
    /// inert outside Colab, so they do not make the output non-simple.
    colab: bool,
    footer: Vec<String>,
    outer_text: TextBuf,
    outer: Vec<(&'a str, ElemAttrs<'a>)>,
    /// Inside an ignored subtree (`display: none`, `<svg>`, `<template>`, a
    /// second table, ...): its element name and nesting depth.
    skip: Option<(&'a str, u32)>,
    any_styled: bool,
    tables_seen: u32,

    table_attrs: ElemAttrs<'a>,
    caption: Option<String>,
    caption_buf: Option<TextBuf>,
    sections: Vec<RawSection<'a>>,
    cur_section: Option<usize>,
    rows: Vec<RawRow<'a>>,
    row_open: bool,
    row_pre: bool,
    /// Tables nested inside the current cell (flattened to text).
    nested: u32,

    // The open cell is the last cell of the last row.
    cell_open: bool,
    buf: TextBuf,
    cell_pre: bool,
    bold_depth: u32,
    italic_depth: u32,
    pre_depth: u32,
    text_bold: bool,
    text_plain_b: bool,
    text_italic: bool,
    text_plain_i: bool,
}

pub(crate) fn build(html: &str) -> Option<Table> {
    find_ascii_ci(html.as_bytes(), b"<table")?;
    let mut b = Builder::new(memchr::memmem::find(html.as_bytes(), b"colab-df-container").is_some());
    for tok in Tokenizer::new(html) {
        b.token(tok);
    }
    b.finish()
}

impl<'a> Builder<'a> {
    fn new(colab: bool) -> Self {
        Builder {
            sheet: StyleSheet::default(),
            phase: Phase::Before,
            standalone: true,
            colab,
            footer: Vec::new(),
            outer_text: TextBuf::default(),
            outer: Vec::new(),
            skip: None,
            any_styled: false,
            tables_seen: 0,
            table_attrs: ElemAttrs::default(),
            caption: None,
            caption_buf: None,
            sections: Vec::new(),
            cur_section: None,
            rows: Vec::new(),
            row_open: false,
            row_pre: false,
            nested: 0,
            cell_open: false,
            buf: TextBuf::default(),
            cell_pre: false,
            bold_depth: 0,
            italic_depth: 0,
            pre_depth: 0,
            text_bold: false,
            text_plain_b: false,
            text_italic: false,
            text_plain_i: false,
        }
    }

    fn token(&mut self, tok: Token<'a>) {
        // Style sheets apply wherever they are, even in hidden subtrees.
        if let Token::Raw { tag: Tag::Style, text } = tok {
            self.sheet.add(text);
            return;
        }
        if let Some((name, depth)) = &mut self.skip {
            match tok {
                Token::Start { tag, name: n, self_closing, .. }
                    if n.eq_ignore_ascii_case(name) && !self_closing && !tag.is_void(n.as_bytes()) =>
                {
                    *depth += 1
                }
                Token::End { name: n, .. } if n.eq_ignore_ascii_case(name) => {
                    *depth -= 1;
                    if *depth == 0 {
                        self.skip = None;
                    }
                }
                _ => {}
            }
            return;
        }
        match tok {
            Token::Raw { .. } => {}
            Token::Text(t) => match self.phase {
                Phase::In => self.table_text(t),
                _ => self.outer_text.push_html(t),
            },
            Token::Start { tag, name, attrs, self_closing } => self.start(tag, name, attrs, self_closing),
            Token::End { tag, name } => match self.phase {
                Phase::In => self.table_end(tag),
                _ => self.outer_end(name),
            },
        }
    }

    fn start(&mut self, tag: Tag, name: &'a str, raw_attrs: &'a str, self_closing: bool) {
        let attrs = read_attrs(raw_attrs);
        if self.phase == Phase::In && matches!(tag, Tag::Td | Tag::Th | Tag::Tr) && !attrs.hidden {
            // Hot path: the cells and rows themselves.
            self.table_start(tag, name, attrs, false);
            return;
        }
        let foreign_self_closing = self_closing && matches!(tag, Tag::Svg | Tag::Math | Tag::Other);
        let void = tag.is_void(name.as_bytes()) || foreign_self_closing;
        if attrs.hidden && !void && !matches!(tag, Tag::Style | Tag::Script | Tag::RawOther | Tag::Plaintext) {
            if tag == Tag::Tr && self.phase == Phase::In && self.nested == 0 {
                self.end_caption();
                self.close_row();
                self.open_row(attrs);
                if let Some(r) = self.rows.last_mut() {
                    r.hidden = true;
                }
                self.row_open = false;
            }
            self.skip = Some((name, 1));
            return;
        }
        match tag {
            Tag::Template => {
                if !void {
                    self.skip = Some((name, 1));
                }
                return;
            }
            Tag::Button | Tag::Svg | Tag::Math => {
                if !self.colab {
                    self.standalone = false;
                }
                if !void {
                    self.skip = Some((name, 1));
                }
                return;
            }
            Tag::Script => {
                if !self.colab {
                    self.standalone = false;
                }
                return;
            }
            Tag::Media | Tag::Hr | Tag::Plaintext | Tag::Other => self.standalone = false,
            Tag::RawOther => {
                if !(name.eq_ignore_ascii_case("title") || name.eq_ignore_ascii_case("noscript")) {
                    self.standalone = false;
                }
                return;
            }
            Tag::Style => return,
            _ => {}
        }
        match self.phase {
            Phase::In => self.table_start(tag, name, attrs, void),
            _ => self.outer_start(tag, name, attrs, void),
        }
    }

    // ---- outside the table -------------------------------------------------

    fn flush_outer_text(&mut self) {
        if self.outer_text.s.is_empty() {
            self.outer_text = TextBuf::default();
            return;
        }
        let t = std::mem::take(&mut self.outer_text).finish();
        if t.is_empty() {
            return;
        }
        if looks_like_shape(&t) {
            self.footer.push(t);
        } else {
            self.standalone = false;
        }
    }

    fn outer_start(&mut self, tag: Tag, name: &'a str, attrs: ElemAttrs<'a>, void: bool) {
        self.flush_outer_text();
        if tag == Tag::Table {
            self.tables_seen += 1;
            if self.phase == Phase::Before {
                self.phase = Phase::In;
                self.any_styled |= attrs.styled();
                self.table_attrs = attrs;
            } else {
                // A second table: ours is not the whole output.
                self.standalone = false;
                if !void {
                    self.skip = Some((name, 1));
                }
            }
            return;
        }
        if !void && self.phase == Phase::Before && self.outer.len() < MAX_OUTER {
            self.outer.push((name, attrs));
        }
    }

    fn outer_end(&mut self, name: &str) {
        self.flush_outer_text();
        if self.phase == Phase::Before
            && let Some(i) = self.outer.iter().rposition(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            self.outer.truncate(i);
        }
    }

    // ---- inside the table ----------------------------------------------------

    fn sink(&mut self) -> Option<&mut TextBuf> {
        if self.cell_open { Some(&mut self.buf) } else { self.caption_buf.as_mut() }
    }

    fn table_text(&mut self, t: &str) {
        if self.cell_open {
            if has_non_ws(t) {
                if self.bold_depth > 0 {
                    self.text_bold = true;
                } else {
                    self.text_plain_b = true;
                }
                if self.italic_depth > 0 {
                    self.text_italic = true;
                } else {
                    self.text_plain_i = true;
                }
            }
            self.buf.push_html(t);
        } else if let Some(cap) = &mut self.caption_buf {
            cap.push_html(t);
        } else if has_non_ws(t) {
            // Text in table context is moved before the table by browsers.
            self.standalone = false;
        }
    }

    fn inline_start(&mut self, tag: Tag) {
        if self.cell_open {
            match tag {
                Tag::B | Tag::Strong => self.bold_depth += 1,
                Tag::I | Tag::Em => self.italic_depth += 1,
                Tag::Pre => {
                    self.pre_depth += 1;
                    self.buf.block_break();
                    self.buf.pre = true;
                    return;
                }
                _ => {}
            }
        }
        if let Some(s) = self.sink() {
            match tag {
                Tag::Br => s.newline(),
                Tag::Block | Tag::P | Tag::Div | Tag::Pre => s.block_break(),
                _ => {}
            }
        }
    }

    fn inline_end(&mut self, tag: Tag) {
        if self.cell_open {
            match tag {
                Tag::B | Tag::Strong => self.bold_depth = self.bold_depth.saturating_sub(1),
                Tag::I | Tag::Em => self.italic_depth = self.italic_depth.saturating_sub(1),
                Tag::Pre => {
                    self.pre_depth = self.pre_depth.saturating_sub(1);
                    if self.pre_depth == 0 {
                        self.buf.pre = self.cell_pre;
                    }
                }
                _ => {}
            }
        }
        if let Some(s) = self.sink() {
            match tag {
                // `</br>` is a `<br>` in browsers.
                Tag::Br => s.newline(),
                Tag::Block | Tag::P | Tag::Div | Tag::Pre => s.block_break(),
                _ => {}
            }
        }
    }

    fn table_start(&mut self, tag: Tag, name: &'a str, attrs: ElemAttrs<'a>, void: bool) {
        if self.nested > 0 {
            // A table inside a cell: flatten its text into the cell.
            match tag {
                Tag::Table => self.nested += 1,
                Tag::Tr | Tag::Caption => self.buf.block_break(),
                Tag::Td | Tag::Th => self.buf.push_html(" "),
                _ => self.inline_start(tag),
            }
            return;
        }
        match tag {
            Tag::Table => {
                if self.cell_open {
                    self.nested = 1;
                    self.standalone = false;
                    self.buf.block_break();
                } else {
                    // `<table>` in table context ends this table and starts another.
                    self.close_table();
                    self.phase = Phase::After;
                    self.tables_seen += 1;
                    self.standalone = false;
                    if !void {
                        self.skip = Some((name, 1));
                    }
                }
            }
            Tag::Caption => {
                self.close_row();
                self.end_caption();
                self.caption_buf = Some(TextBuf::default());
            }
            Tag::Colgroup | Tag::Col => {
                self.end_caption();
                self.close_row();
            }
            Tag::Thead | Tag::Tbody | Tag::Tfoot => {
                self.end_caption();
                self.close_row();
                let (kind, tag) = match tag {
                    Tag::Thead => (SecKind::Head, "thead"),
                    Tag::Tfoot => (SecKind::Foot, "tfoot"),
                    _ => (SecKind::Body, "tbody"),
                };
                self.any_styled |= attrs.styled();
                self.sections.push(RawSection { kind, attrs, tag, pre: None, plain_row_pre: None });
                self.cur_section = Some(self.sections.len() - 1);
            }
            Tag::Tr => {
                self.end_caption();
                self.close_row();
                self.open_row(attrs);
            }
            Tag::Th | Tag::Td => {
                self.end_caption();
                self.close_cell();
                if !self.row_open {
                    self.open_row(ElemAttrs::default());
                }
                self.open_cell(tag == Tag::Th, attrs);
            }
            _ => {
                if self.cell_open || self.caption_buf.is_some() {
                    self.inline_start(tag);
                }
            }
        }
    }

    fn table_end(&mut self, tag: Tag) {
        if self.nested > 0 {
            match tag {
                Tag::Table => {
                    self.nested -= 1;
                    if self.nested == 0 {
                        self.buf.block_break();
                    }
                }
                _ => self.inline_end(tag),
            }
            return;
        }
        match tag {
            Tag::Table => {
                self.close_table();
                self.phase = Phase::After;
            }
            Tag::Caption => self.end_caption(),
            Tag::Thead | Tag::Tbody | Tag::Tfoot => {
                self.close_row();
                self.cur_section = None;
            }
            Tag::Tr => self.close_row(),
            Tag::Th | Tag::Td => self.close_cell(),
            _ => {
                if self.cell_open || self.caption_buf.is_some() {
                    self.inline_end(tag);
                }
            }
        }
    }

    fn end_caption(&mut self) {
        if let Some(buf) = self.caption_buf.take() {
            let t = buf.finish();
            if self.caption.is_none() && !t.is_empty() {
                self.caption = Some(t);
            }
        }
    }

    fn open_row(&mut self, attrs: ElemAttrs<'a>) {
        let section = match self.cur_section {
            Some(s) => s,
            None => {
                self.sections.push(RawSection {
                    kind: SecKind::Body,
                    attrs: ElemAttrs::default(),
                    tag: "tbody",
                    pre: None,
                    plain_row_pre: None,
                });
                let s = self.sections.len() - 1;
                self.cur_section = Some(s);
                s
            }
        };
        self.any_styled |= attrs.styled();
        let cap = self.rows.last().map_or(0, |r| r.cells.len());
        self.rows.push(RawRow { section, attrs, cells: Vec::with_capacity(cap), hidden: false });
        self.row_open = true;
        self.row_pre = if self.sheet.has_white_space || mentions_white_space(attrs.style) {
            self.row_white_space(section, attrs)
        } else {
            false
        };
    }

    /// Ancestors of the table's rows: wrappers, then `<table>`.
    fn outer_chain(&self) -> (Vec<Elem<'a>>, Vec<&'a str>) {
        let mut chain: Vec<Elem<'a>> = self.outer.iter().map(|(n, a)| a.elem(n)).collect();
        let mut inline: Vec<&'a str> = self.outer.iter().map(|(_, a)| a.style).collect();
        chain.push(self.table_attrs.elem("table"));
        inline.push(self.table_attrs.style);
        (chain, inline)
    }

    /// `white-space` of a new row: its own, else inherited (cached per section).
    fn row_white_space(&mut self, section: usize, attrs: ElemAttrs<'a>) -> bool {
        let plain = attrs.id.is_empty() && attrs.class.is_empty() && attrs.style.is_empty() && !self.sheet.structural;
        if plain && let Some(p) = self.sections.get(section).and_then(|s| s.plain_row_pre) {
            return p;
        }
        let v = self.row_white_space_uncached(section, attrs);
        if plain && let Some(s) = self.sections.get_mut(section) {
            s.plain_row_pre = Some(v);
        }
        v
    }

    fn row_white_space_uncached(&mut self, section: usize, attrs: ElemAttrs<'a>) -> bool {
        let (mut chain, mut inline) = self.outer_chain();
        let Some(sec) = self.sections.get(section) else { return false };
        chain.push(sec.attrs.elem(sec.tag));
        inline.push(sec.attrs.style);
        let index = self.rows.iter().rev().take_while(|r| r.section == section).count() as u32;
        chain.push(Elem { index, ..attrs.elem("tr") });
        let mut c = Computed::default();
        self.sheet.compute(&chain, &attrs.style(), &mut c);
        if let Some(p) = c.pre.val {
            return p;
        }
        chain.pop();
        if let Some(p) = sec.pre {
            return p;
        }
        let mut v = false;
        for depth in (1..=chain.len()).rev() {
            let mut c = Computed::default();
            self.sheet.compute(&chain[..depth], &decode(inline[depth - 1]), &mut c);
            if let Some(p) = c.pre.val {
                v = p;
                break;
            }
        }
        if let Some(sec) = self.sections.get_mut(section) {
            sec.pre = Some(v);
        }
        v
    }

    fn open_cell(&mut self, th: bool, attrs: ElemAttrs<'a>) {
        let row_number = !th && attrs.has_class("rowNumber");
        let mut pre = self.row_pre;
        if self.sheet.ws_cells || mentions_white_space(attrs.style) {
            let (mut chain, _) = self.outer_chain();
            if let Some(s) = self.cur_section.and_then(|s| self.sections.get(s)) {
                chain.push(s.attrs.elem(s.tag));
            }
            if let Some(r) = self.rows.last() {
                chain.push(r.attrs.elem("tr"));
                let index = r.cells.len() as u32 + 1;
                chain.push(Elem { index, ..attrs.elem(if th { "th" } else { "td" }) });
            }
            let mut c = Computed::default();
            self.sheet.compute(&chain, &attrs.style(), &mut c);
            if let Some(p) = c.pre.val {
                pre = p;
            }
        }
        self.any_styled |= attrs.styled();
        let Some(row) = self.rows.last_mut() else { return };
        row.cells.push(RawCell {
            text: String::new(),
            attrs,
            th_tag: th,
            header: th || row_number,
            bold_tag: false,
            italic_tag: false,
            style: CellStyle::default(),
            align: None,
        });
        self.cell_open = true;
        self.buf = TextBuf::with_pre(pre);
        self.cell_pre = pre;
        self.bold_depth = 0;
        self.italic_depth = 0;
        self.pre_depth = 0;
        self.text_bold = false;
        self.text_plain_b = false;
        self.text_italic = false;
        self.text_plain_i = false;
    }

    fn close_cell(&mut self) {
        if !self.cell_open {
            return;
        }
        self.cell_open = false;
        self.nested = 0;
        let text = std::mem::take(&mut self.buf).finish();
        if let Some(cell) = self.rows.last_mut().and_then(|r| r.cells.last_mut()) {
            cell.text = text;
            cell.bold_tag = self.text_bold && !self.text_plain_b;
            cell.italic_tag = self.text_italic && !self.text_plain_i;
        }
    }

    fn close_row(&mut self) {
        self.close_cell();
        self.row_open = false;
    }

    fn close_table(&mut self) {
        self.end_caption();
        self.close_row();
        self.nested = 0;
    }

    fn finish(mut self) -> Option<Table> {
        match self.phase {
            Phase::Before => return None,
            Phase::In => self.close_table(),
            Phase::After => {}
        }
        self.flush_outer_text();
        if self.tables_seen > 1 {
            self.standalone = false;
        }
        Some(self.assemble())
    }

    // ---- assembly ------------------------------------------------------------

    /// Resolve CSS for every raw cell into `RawCell::style` / `align`.
    fn resolve_styles(&mut self) {
        let any_rules = !self.sheet.is_empty();
        if !any_rules && !self.any_styled {
            for c in self.rows.iter_mut().flat_map(|r| r.cells.iter_mut()) {
                c.style.bold = c.bold_tag;
                c.style.italic = c.italic_tag;
                c.align = c.attrs.align;
            }
            return;
        }
        let (mut chain, _) = self.outer_chain();
        let sheet = &self.sheet;
        let mut table_c = Computed::default();
        sheet.compute(&chain, &self.table_attrs.style(), &mut table_c);
        let sec_c: Vec<Computed> = self
            .sections
            .iter()
            .map(|s| {
                let mut c = Computed::default();
                chain.push(s.attrs.elem(s.tag));
                sheet.compute(&chain, &s.attrs.style(), &mut c);
                chain.pop();
                c
            })
            .collect();
        // Row positions within their section, for :nth-child.
        let mut sec_counts = vec![0u32; self.sections.len()];
        for r in &self.rows {
            if let Some(n) = sec_counts.get_mut(r.section) {
                *n += 1;
            }
        }
        let mut sec_seen = vec![0u32; self.sections.len()];
        let base = chain.len();
        for r in &mut self.rows {
            let Some(sec) = self.sections.get(r.section) else { continue };
            sec_seen[r.section] += 1;
            if r.hidden {
                continue;
            }
            chain.truncate(base);
            chain.push(sec.attrs.elem(sec.tag));
            chain.push(Elem { index: sec_seen[r.section], count: sec_counts[r.section], ..r.attrs.elem("tr") });
            let mut row_c = Computed::default();
            if (any_rules && sheet.may_match("tr", r.attrs.id, r.attrs.class)) || !r.attrs.style.is_empty() {
                sheet.compute(&chain, &r.attrs.style(), &mut row_c);
            }
            let sc = &sec_c[r.section];
            let row_levels = [&row_c, sc, &table_c];
            let row_bg = row_levels.iter().find_map(|l| l.bg.val.flatten()).or(r.attrs.bgcolor).or(self.table_attrs.bgcolor);
            let row_fg = row_levels.iter().find_map(|l| l.fg.val);
            let row_bold = row_levels.iter().find_map(|l| l.bold.val).unwrap_or(false);
            let row_italic = row_levels.iter().find_map(|l| l.italic.val).unwrap_or(false);
            let n_th = r.cells.iter().filter(|c| c.th_tag).count() as u32;
            let n = r.cells.len() as u32;
            let n_td = n - n_th;
            let (mut i_th, mut i_td) = (0u32, 0u32);
            for (k, c) in r.cells.iter_mut().enumerate() {
                let (index_of_type, count_of_type) = if c.th_tag {
                    i_th += 1;
                    (i_th, n_th)
                } else {
                    i_td += 1;
                    (i_td, n_td)
                };
                let tag = if c.th_tag { "th" } else { "td" };
                if (any_rules && sheet.may_match(tag, c.attrs.id, c.attrs.class)) || !c.attrs.style.is_empty() {
                    let mut cc = Computed::default();
                    chain.push(Elem {
                        index: k as u32 + 1,
                        count: n,
                        index_of_type,
                        count_of_type,
                        ..c.attrs.elem(tag)
                    });
                    sheet.compute(&chain, &c.attrs.style(), &mut cc);
                    chain.pop();
                    c.style = CellStyle {
                        bg: cc.bg.val.flatten().or(c.attrs.bgcolor).or(row_bg),
                        fg: cc.fg.val.or(row_fg),
                        bold: cc.bold.val.unwrap_or(row_bold) || c.bold_tag,
                        italic: cc.italic.val.unwrap_or(row_italic) || c.italic_tag,
                        bar: cc.bar.val.flatten(),
                    };
                    c.align = cc.align.val.or(c.attrs.align);
                } else {
                    c.style = CellStyle {
                        bg: c.attrs.bgcolor.or(row_bg),
                        fg: row_fg,
                        bold: row_bold || c.bold_tag,
                        italic: row_italic || c.italic_tag,
                        bar: None,
                    };
                    c.align = c.attrs.align;
                }
            }
        }
    }

    fn assemble(mut self) -> Table {
        self.resolve_styles();
        let footer = (!self.footer.is_empty()).then(|| self.footer.join(" · "));
        let polars = footer.as_deref().is_some_and(|f| f.starts_with("shape:"));

        // Row groups (rowspans never cross them), in display order.
        let mut head: Vec<Vec<RawRow>> = Vec::new();
        let mut body: Vec<Vec<RawRow>> = Vec::new();
        let mut foot: Vec<Vec<RawRow>> = Vec::new();
        let mut last_section = usize::MAX;
        for r in std::mem::take(&mut self.rows) {
            if r.hidden {
                continue;
            }
            let kind = self.sections.get(r.section).map_or(SecKind::Body, |s| s.kind);
            let groups = match kind {
                SecKind::Head => &mut head,
                SecKind::Body => &mut body,
                SecKind::Foot => &mut foot,
            };
            if r.section != last_section || groups.is_empty() {
                groups.push(Vec::new());
            }
            last_section = r.section;
            if let Some(g) = groups.last_mut() {
                g.push(r);
            }
        }
        body.extend(foot);

        // No <thead>: leading all-<th> rows are the header (if data follows).
        if head.is_empty() && !body.is_empty() {
            let g = &body[0];
            let k = g.iter().take_while(|r| !r.cells.is_empty() && r.cells.iter().all(|c| c.th_tag)).count();
            if k > 0 && (k < g.len() || body.len() > 1) {
                let rest = body[0].split_off(k);
                let promoted = std::mem::replace(&mut body[0], rest);
                if body[0].is_empty() {
                    body.remove(0);
                }
                head.push(promoted);
            }
        }

        // Dtype row: polars (all <td> in thead), Julia (`subheader`), R (`<dbl>`).
        let head_rows: usize = head.iter().map(Vec::len).sum();
        let mut dtype_row = None;
        let mut flavor_r = false;
        if let Some(last) = head.last().and_then(|g| g.last()) {
            let polars_row = polars && !last.cells.is_empty() && last.cells.iter().all(|c| !c.th_tag);
            let julia_row = last.attrs.has_class("subheader");
            let mut nonempty = last.cells.iter().filter(|c| !c.text.is_empty()).peekable();
            let r_row = head_rows >= 2 && nonempty.peek().is_some() && nonempty.all(|c| is_r_type(&c.text));
            if polars_row || julia_row || r_row {
                flavor_r = r_row && !polars_row && !julia_row;
                dtype_row = head.last_mut().and_then(|g| g.pop());
                if head.last().is_some_and(|g| g.is_empty()) {
                    head.pop();
                }
            }
        }

        let mut header = Grid::default();
        for g in head {
            header.expand(g);
        }
        let mut grid = Grid::default();
        for g in body {
            grid.expand(g);
        }
        let dtypes_cells = dtype_row.map(|r| {
            let mut g = Grid::default();
            g.expand(vec![r]);
            g.rows.pop().unwrap_or_default()
        });

        // Truncation-marker rows never widen the grid: pandas' Styler emits a
        // trim row with one `...` per *hidden* column too.
        let width = header
            .rows
            .iter()
            .chain(grid.rows.iter().filter(|r| !all_ellipsis(r)))
            .map(Vec::len)
            .chain(dtypes_cells.iter().map(Vec::len))
            .max()
            .unwrap_or(0);
        if width > 0 {
            for (row, over) in grid.rows.iter_mut().zip(grid.overrides.iter_mut()) {
                if row.len() > width && all_ellipsis(row) {
                    row.truncate(width);
                    over.truncate(width);
                }
            }
        }
        let columns = header
            .rows
            .iter()
            .chain(&grid.rows)
            .map(Vec::len)
            .chain(dtypes_cells.iter().map(Vec::len))
            .max()
            .unwrap_or(0)
            .min(MAX_COLUMNS);

        // Safety cap on total grid size.
        let mut dropped_rows = 0;
        if let Some(per_row) = MAX_GRID_CELLS.checked_div(columns) {
            let max_rows = per_row.saturating_sub(header.rows.len());
            if grid.rows.len() > max_rows {
                dropped_rows = grid.rows.len() - max_rows;
                grid.rows.truncate(max_rows);
                grid.overrides.truncate(max_rows);
            }
        }
        header.pad(columns, true);
        grid.pad(columns, false);

        let index_columns = index_columns(&header.rows, &grid.rows).min(columns);

        // Column statistics over body data cells: (numbers, texts, nulls).
        let mut stats = vec![(0usize, 0usize, 0usize); columns];
        for row in &grid.rows {
            for (c, cell) in row.iter().enumerate().skip(index_columns) {
                match cell.kind {
                    CellKind::Number => stats[c].0 += 1,
                    CellKind::Text | CellKind::Bool => stats[c].1 += 1,
                    CellKind::Null => stats[c].2 += 1,
                    _ => {}
                }
            }
        }
        let numeric_col = |c: usize| {
            let (n, t, z) = stats.get(c).copied().unwrap_or_default();
            (n > 0 && n >= t) || (n == 0 && t == 0 && z > 0)
        };

        // Alignment and is_index.
        for (r, row) in grid.rows.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                cell.is_index = c < index_columns;
                if cell.span_origin.is_some() {
                    continue;
                }
                cell.align = match grid.overrides.get(r).and_then(|o| o.get(c)).copied().flatten() {
                    Some(a) => a,
                    None if cell.is_index => Align::Left,
                    None => match cell.kind {
                        CellKind::Number => Align::Right,
                        CellKind::Ellipsis => Align::Center,
                        CellKind::Null | CellKind::Empty if numeric_col(c) => Align::Right,
                        _ => Align::Left,
                    },
                };
            }
        }
        for (r, row) in header.rows.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                cell.is_index = c < index_columns;
                if cell.span_origin.is_some() {
                    continue;
                }
                cell.align = match header.overrides.get(r).and_then(|o| o.get(c)).copied().flatten() {
                    Some(a) => a,
                    None if cell.is_index => Align::Left,
                    None if cell.kind == CellKind::Ellipsis => Align::Center,
                    None if cell.colspan > 1 => Align::Center,
                    None if numeric_col(c) => Align::Right,
                    None => Align::Left,
                };
            }
        }
        header.finish_covered();
        grid.finish_covered();

        let dtypes = dtypes_cells.map(|cells| {
            let mut v: Vec<String> = cells.into_iter().map(|c| c.text).collect();
            v.resize(columns, String::new());
            for t in v.iter_mut().take(index_columns) {
                t.clear();
            }
            v
        });

        let t = &self.table_attrs;
        let flavor = if polars {
            Flavor::Polars
        } else if t.id.starts_with("T_") {
            Flavor::PandasStyler
        } else if t.has_class("data-frame") {
            Flavor::Julia
        } else if t.has_class("dataframe") {
            let cap = self.caption.as_deref().unwrap_or("");
            let r_caption = ["A data.frame", "A tibble", "A data.table", "A matrix", "A grouped_df"];
            if flavor_r || r_caption.iter().any(|p| cap.starts_with(p)) { Flavor::R } else { Flavor::Pandas }
        } else {
            Flavor::Generic
        };

        Table {
            caption: self.caption,
            header: header.rows,
            body: grid.rows,
            index_columns,
            columns,
            footer,
            dtypes,
            flavor,
            standalone: self.standalone,
            dropped_rows,
        }
    }
}

/// Leading header-like cells shared by the body rows (pandas index levels,
/// R row names, Julia row numbers).
fn index_columns(header: &[Vec<Cell>], body: &[Vec<Cell>]) -> usize {
    let lead = |row: &Vec<Cell>| {
        row.iter()
            .take_while(|c| match c.span_origin {
                Some((r, k)) => body.get(r).and_then(|x| x.get(k)).is_some_and(|o| o.is_header),
                None => c.is_header,
            })
            .count()
    };
    if body.is_empty() {
        // No data rows (empty DataFrame): pandas' blank corner cells.
        return header.first().map_or(0, |first| {
            first
                .iter()
                .take_while(|c| c.is_header && c.kind == CellKind::Empty && c.span_origin.is_none())
                .count()
        });
    }
    // Rows that are all labels (a pandas frame without columns) do not vote.
    let data_rows = body.iter().map(|r| (lead(r), r.len())).filter(|&(n, len)| n < len).map(|(n, _)| n).min();
    data_rows.unwrap_or_else(|| body.iter().map(lead).min().unwrap_or(0))
}

fn all_ellipsis(row: &[Cell]) -> bool {
    let mut any = false;
    for c in row {
        match c.kind {
            CellKind::Ellipsis => any = true,
            CellKind::Empty => {}
            _ => return false,
        }
    }
    any
}

/// R / IRkernel column type labels: `<dbl>`, `<chr>`, `<fct>`, `<S3: POSIXct>`.
fn is_r_type(t: &str) -> bool {
    t.len() >= 3
        && t.len() <= 32
        && t.starts_with('<')
        && t.ends_with('>')
        && !t[1..t.len() - 1].contains(['<', '>'])
}

/// A dimensions line printed next to a table.
fn looks_like_shape(t: &str) -> bool {
    let t = t.trim();
    if t.len() > 160 {
        return false;
    }
    if let Some(rest) = t.strip_prefix("shape:") {
        return rest.trim_start().starts_with('(');
    }
    let s = t.trim_start_matches('[').trim_end_matches(']');
    let w: Vec<&str> = s.split_whitespace().collect();
    let num = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit() || b == b',');
    let by = |x: &str| matches!(x, "×" | "x" | "X");
    let dims = |x: &str| {
        let mut parts = x.split(['×', 'x']);
        matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if num(a) && num(b))
    };
    match w.as_slice() {
        [n, r, b, m, c] if num(n) && by(b) && num(m) => {
            matches!(*r, "rows" | "row") && matches!(*c, "columns" | "column" | "cols")
        }
        [n, rest @ ..] if num(n) && rest.last() == Some(&"omitted") && rest.len() <= 6 => true,
        [first, rest @ ..] if dims(first) && rest.len() <= 3 => true,
        ["only", "showing", "top", n, r] if num(n) => r.starts_with("row"),
        _ => false,
    }
}

/// Row groups expanded into grid slots.
#[derive(Default)]
struct Grid {
    rows: Vec<Vec<Cell>>,
    /// Alignment requested by CSS / attributes, per slot.
    overrides: Vec<Vec<Option<Align>>>,
}

impl Grid {
    /// Place one row group (`<thead>` / `<tbody>`): rowspans stay inside it.
    fn expand(&mut self, group: Vec<RawRow>) {
        let base = self.rows.len();
        let n = group.len();
        // Per column: (rows still covered, origin row, origin col).
        let mut carry: Vec<(u32, usize, usize)> = Vec::new();
        for (ri, row) in group.into_iter().enumerate() {
            let r_abs = base + ri;
            let mut slots: Vec<Cell> = Vec::with_capacity(row.cells.len().max(carry.len()));
            let mut over: Vec<Option<Align>> = Vec::new();
            for (c, cr) in carry.iter_mut().enumerate() {
                if cr.0 > 0 {
                    cr.0 -= 1;
                    if slots.len() <= c {
                        slots.resize_with(c + 1, Cell::default);
                    }
                    slots[c] = covered(cr.1, cr.2);
                }
            }
            let mut col = 0;
            for cell in row.cells {
                while col < slots.len() && taken(&slots[col]) {
                    col += 1;
                }
                if col >= MAX_COLUMNS {
                    break;
                }
                let mut cs = (cell.attrs.colspan.max(1) as usize).min(MAX_COLUMNS - col);
                for k in 1..cs {
                    if slots.get(col + k).is_some_and(taken) {
                        cs = k;
                        break;
                    }
                }
                let left = n - ri;
                let rs = if cell.attrs.rowspan == 0 { left } else { (cell.attrs.rowspan as usize).min(left) }.max(1);
                if slots.len() < col + cs {
                    slots.resize_with(col + cs, Cell::default);
                }
                if let Some(a) = cell.align {
                    if over.len() <= col {
                        over.resize(col + 1, None);
                    }
                    over[col] = Some(a);
                }
                let kind = classify(&cell.text, cell.style.italic);
                slots[col] = Cell {
                    text: cell.text,
                    colspan: cs as u16,
                    rowspan: rs.min(u16::MAX as usize) as u16,
                    span_origin: None,
                    is_header: cell.header,
                    is_index: false,
                    align: Align::Left,
                    kind,
                    style: cell.style,
                };
                for slot in slots.iter_mut().skip(col + 1).take(cs - 1) {
                    *slot = covered(r_abs, col);
                }
                if rs > 1 {
                    if carry.len() < col + cs {
                        carry.resize(col + cs, (0, 0, 0));
                    }
                    for cr in carry.iter_mut().skip(col).take(cs) {
                        *cr = (rs as u32 - 1, r_abs, col);
                    }
                }
                col += cs;
            }
            // Slots no cell reached stay default (gap) cells: colspan 0, fixed in `pad`.
            self.rows.push(slots);
            self.overrides.push(over);
        }
    }

    fn pad(&mut self, columns: usize, header: bool) {
        let n = self.rows.len();
        for (r, row) in self.rows.iter_mut().enumerate() {
            row.truncate(columns);
            row.resize_with(columns, Cell::default);
            for (c, cell) in row.iter_mut().enumerate() {
                if cell.span_origin.is_some() {
                    continue;
                }
                if cell.colspan == 0 {
                    // A gap or padding slot.
                    cell.is_header = header;
                }
                cell.colspan = cell.colspan.clamp(1, (columns - c).min(u16::MAX as usize) as u16);
                cell.rowspan = cell.rowspan.clamp(1, (n - r).min(u16::MAX as usize) as u16);
            }
        }
    }

    /// Covered slots take their origin's look.
    fn finish_covered(&mut self) {
        for r in 0..self.rows.len() {
            for c in 0..self.rows[r].len() {
                let Some((or, oc)) = self.rows[r][c].span_origin else { continue };
                let origin = self.rows.get(or).and_then(|x| x.get(oc)).map(|o| (o.style, o.is_header, o.align));
                let cell = &mut self.rows[r][c];
                match origin {
                    Some((style, is_header, align)) => {
                        cell.style = CellStyle { bar: None, ..style };
                        cell.is_header = is_header;
                        cell.align = align;
                    }
                    None => {
                        cell.span_origin = None;
                        cell.colspan = 1;
                        cell.rowspan = 1;
                    }
                }
            }
        }
    }
}

/// A slot already holds a cell or is covered by a span.
fn taken(c: &Cell) -> bool {
    c.colspan != 0 || c.span_origin.is_some()
}

fn covered(r: usize, c: usize) -> Cell {
    Cell { span_origin: Some((r, c)), ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        for s in [
            "60 rows × 50 columns",
            "1 row × 3 columns",
            "[5 rows x 3 columns]",
            "shape: (5, 4)",
            "shape: (3,)",
            "3×2 DataFrame",
            "975 rows omitted",
            "only showing top 20 rows",
        ] {
            assert!(looks_like_shape(s), "{s}");
        }
        for s in ["hello", "Total: 5", "60 rows", "x × y", "rows × columns"] {
            assert!(!looks_like_shape(s), "{s}");
        }
    }

    #[test]
    fn spans_attr() {
        assert_eq!(parse_span(" 2px"), Some(2));
        assert_eq!(parse_span("x"), None);
        assert_eq!(parse_span("99999999"), Some(65535));
        assert!(is_display_none("color: red; DISPLAY : none"));
        assert!(!is_display_none("display: block"));
    }
}
