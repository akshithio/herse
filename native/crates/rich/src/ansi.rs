//! ANSI escape sequences -> plain text + styled spans.
//!
//! [`parse`] makes one pass over the input (no regex, no per-character
//! allocation): printable runs are copied in bulk, SGR (`CSI … m`) updates the
//! current [`AnsiStyle`], OSC 8 hyperlinks become [`Link`]s, and every other
//! escape (CSI, OSC, DCS/SOS/PM/APC strings, two- and three-byte escapes, lone
//! `ESC`) plus C0/C1 control characters other than `\t` and `\n` are removed.
//!
//! Run [`crate::stream::normalize`] first for stream text: `\r` and `\b` have
//! terminal meaning there and this module simply drops them.
//!
//! SGR coverage: `0` (and empty), `1` bold, `2` dim, `3` italic, `4` underline
//! (`4:0` turns it off, `4:1`..`4:5` on), `7` inverse, `9` strikethrough,
//! `21`..`29` resets (`21` = bold off as in JupyterLab/nbconvert, not double
//! underline), `30`..`37`/`90`..`97` and `40`..`47`/`100`..`107` named colors,
//! `38`/`48` extended colors in both `;` (`38;5;n`, `38;2;r;g;b`) and `:`
//! (`38:5:n`, `38:2::r:g:b`, `38:2:r:g:b`) forms, `39`/`49` default colors,
//! `58`/`59` (underline color, consumed and ignored). Combined parameters in one
//! sequence apply left to right. As in JupyterLab, a malformed extended color
//! drops the rest of that sequence.
//!
//! Colors stay symbolic ([`AnsiColor`]); [`AnsiStyle::resolve`] maps them through a
//! [`Palette`] with JupyterLab's rendering rules (bold brightens colors 0–7,
//! inverse swaps, yellow-on-yellow fix).

use std::borrow::Cow;
use std::ops::Range;

/// A color as written in the escape sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnsiColor {
    /// One of the 16 theme colors: `0..=7` from `30`–`37`/`40`–`47`,
    /// `8..=15` from `90`–`97`/`100`–`107`.
    Named(u8),
    /// `38;5;n` / `48;5;n`. Indices below 16 are the theme colors again (the
    /// palette decides), 16–231 the 6×6×6 cube, 232–255 the gray ramp.
    Indexed(u8),
    /// `38;2;r;g;b` / `48;2;r;g;b`.
    Rgb(u8, u8, u8),
}

/// The SGR state for a run of text. `Default` is "no styling".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AnsiStyle {
    pub fg: Option<AnsiColor>,
    pub bg: Option<AnsiColor>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
}

impl AnsiStyle {
    /// True when the style draws exactly like unstyled text.
    pub fn is_default(&self) -> bool {
        *self == AnsiStyle::default()
    }
}

/// A styled byte range of [`StyledText::text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    pub range: Range<usize>,
    pub style: AnsiStyle,
}

/// An OSC 8 hyperlink over a byte range of [`StyledText::text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub range: Range<usize>,
    pub url: String,
}

/// Output of [`parse`].
///
/// `spans` hold only non-default styles; they are sorted, non-overlapping,
/// non-empty, and adjacent spans always differ in style. Text not covered by a
/// span is unstyled ([`StyledText::runs`] yields the gaps too). All ranges are
/// byte ranges on `char` boundaries of `text`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StyledText {
    pub text: String,
    pub spans: Vec<StyledSpan>,
    pub links: Vec<Link>,
}

impl StyledText {
    /// True when nothing in the text is styled or linked.
    pub fn is_plain(&self) -> bool {
        self.spans.is_empty() && self.links.is_empty()
    }

    /// Contiguous runs covering the whole text, unstyled gaps included, in
    /// order. Handy for building one text run per style.
    pub fn runs(&self) -> impl Iterator<Item = (Range<usize>, AnsiStyle)> + '_ {
        let mut pos = 0;
        let mut spans = self.spans.iter().peekable();
        let len = self.text.len();
        std::iter::from_fn(move || {
            if pos >= len {
                return None;
            }
            match spans.peek() {
                Some(s) if s.range.start == pos => {
                    let s = spans.next().unwrap();
                    pos = s.range.end;
                    Some((s.range.clone(), s.style))
                }
                Some(s) => {
                    let r = pos..s.range.start;
                    pos = s.range.start;
                    Some((r, AnsiStyle::default()))
                }
                None => {
                    let r = pos..len;
                    pos = len;
                    Some((r, AnsiStyle::default()))
                }
            }
        })
    }
}

// Byte classes for the scanner.
const PLAIN: u8 = 0;
const SPECIAL: u8 = 1;

static CLASS: [u8; 256] = {
    let mut t = [PLAIN; 256];
    let mut i = 0;
    while i < 0x20 {
        if i != b'\t' as usize && i != b'\n' as usize {
            t[i] = SPECIAL;
        }
        i += 1;
    }
    t[0x7f] = SPECIAL;
    // Lead byte of U+0080..U+00BF; only U+0080..U+009F (C1 controls) are
    // special, decided by looking at the continuation byte.
    t[0xc2] = SPECIAL;
    t
};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Parse `text`, removing escape sequences and recording styles and links.
pub fn parse(text: &str) -> StyledText {
    let mut p = Parser {
        out: StyledText {
            text: String::with_capacity(text.len()),
            spans: Vec::new(),
            links: Vec::new(),
        },
        style: AnsiStyle::default(),
        link: None,
    };
    p.run(text);
    if let Some((start, url)) = p.link.take() {
        p.close_link(start, url);
    }
    p.out
}

/// `text` without escape sequences and control characters (other than `\t`
/// and `\n`). Borrowed when there is nothing to remove.
pub fn strip(text: &str) -> Cow<'_, str> {
    if !needs_work(text.as_bytes()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    scan(text, &mut |ev| {
        if let Event::Text(s) = ev {
            out.push_str(s);
        }
    });
    Cow::Owned(out)
}

/// True if `text` contains anything [`parse`]/[`strip`] would act on.
pub fn has_escapes(text: &str) -> bool {
    needs_work(text.as_bytes())
}

fn needs_work(b: &[u8]) -> bool {
    let mut i = 0;
    loop {
        i = next_special(b, i);
        if i >= b.len() {
            return false;
        }
        if b[i] != 0xc2 || matches!(b.get(i + 1), Some(0x80..=0x9f)) {
            return true;
        }
        i += 1;
    }
}

/// Index of the next byte whose class is `SPECIAL` at or after `i` (or
/// `b.len()`), eight bytes at a time: printable runs are the common case.
#[inline]
fn next_special(b: &[u8], mut i: usize) -> usize {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    #[inline]
    fn has_zero(y: u64) -> u64 {
        y.wrapping_sub(LO) & !y & HI
    }
    while i + 8 <= b.len() {
        let x = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        // Any byte < 0x20, == 0x7f or == 0xc2?
        let maybe = (x.wrapping_sub(LO * 0x20) & !x & HI)
            | has_zero(x ^ (LO * 0x7f))
            | has_zero(x ^ (LO * 0xc2));
        if maybe != 0 {
            // Tabs and newlines also trip the test; look at the bytes.
            for k in i..i + 8 {
                if CLASS[b[k] as usize] == SPECIAL {
                    return k;
                }
            }
        }
        i += 8;
    }
    while i < b.len() && CLASS[b[i] as usize] == PLAIN {
        i += 1;
    }
    i
}

struct Parser {
    out: StyledText,
    style: AnsiStyle,
    link: Option<(usize, String)>,
}

impl Parser {
    fn run(&mut self, text: &str) {
        scan(text, &mut |ev| match ev {
            Event::Text(s) => self.push_text(s),
            Event::Sgr(params) => apply_sgr(&mut self.style, params),
            Event::Osc(body) => self.osc(body),
        });
    }

    #[inline]
    fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let start = self.out.text.len();
        self.out.text.push_str(s);
        let end = self.out.text.len();
        if self.style.is_default() {
            return;
        }
        if let Some(last) = self.out.spans.last_mut()
            && last.range.end == start
            && last.style == self.style
        {
            last.range.end = end;
            return;
        }
        self.out.spans.push(StyledSpan {
            range: start..end,
            style: self.style,
        });
    }

    fn osc(&mut self, body: &str) {
        // OSC 8 ; params ; URI
        let Some(rest) = body.strip_prefix("8;") else {
            return;
        };
        let uri = match rest.find(';') {
            Some(i) => &rest[i + 1..],
            None => return,
        };
        let pos = self.out.text.len();
        if let Some((start, url)) = self.link.take() {
            self.close_link(start, url);
        }
        if !uri.is_empty() {
            self.link = Some((pos, uri.to_string()));
        }
    }

    fn close_link(&mut self, start: usize, url: String) {
        let end = self.out.text.len();
        if end > start {
            self.out.links.push(Link {
                range: start..end,
                url,
            });
        }
    }
}

enum Event<'a> {
    Text(&'a str),
    /// Parameter bytes of an SGR sequence (between `CSI` and `m`).
    Sgr(&'a str),
    /// OSC body without introducer and terminator.
    Osc(&'a str),
}

/// The single-pass scanner shared by [`parse`] and [`strip`].
fn scan<'a, F: FnMut(Event<'a>)>(text: &'a str, emit: &mut F) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut plain = 0; // start of the pending plain run
    while i < n {
        i = next_special(b, i);
        if i >= n {
            break;
        }
        let c = b[i];
        if c == 0xc2 {
            match b.get(i + 1) {
                Some(0x80..=0x9f) => {
                    // C1 control: drop it.
                    if i > plain {
                        emit(Event::Text(&text[plain..i]));
                    }
                    i += 2;
                    plain = i;
                }
                _ => i += 1,
            }
            continue;
        }
        // Flush the plain run before this special byte.
        if i > plain {
            emit(Event::Text(&text[plain..i]));
        }
        if c != ESC {
            // Other C0 control or DEL: drop.
            i += 1;
            plain = i;
            continue;
        }
        i = escape(text, i, emit);
        plain = i;
    }
    if n > plain {
        emit(Event::Text(&text[plain..n]));
    }
}

/// Handle the escape starting at `b[start] == ESC`; returns the index after it.
fn escape<'a, F: FnMut(Event<'a>)>(text: &'a str, start: usize, emit: &mut F) -> usize {
    let b = text.as_bytes();
    let n = b.len();
    let i = start + 1;
    if i >= n {
        return n; // lone ESC at the end
    }
    match b[i] {
        b'[' => {
            // CSI: params 0x30-0x3F, intermediates 0x20-0x2F, final 0x40-0x7E.
            let p0 = i + 1;
            let mut j = p0;
            while j < n && (0x30..=0x3f).contains(&b[j]) {
                j += 1;
            }
            let p1 = j;
            while j < n && (0x20..=0x2f).contains(&b[j]) {
                j += 1;
            }
            let inter = j > p1;
            if j < n && (0x40..=0x7e).contains(&b[j]) {
                if b[j] == b'm' && !inter {
                    let params = &text[p0..p1];
                    // Private-marker parameters (`<=>?`) are not SGR.
                    if !matches!(params.as_bytes().first(), Some(b'<' | b'=' | b'>' | b'?')) {
                        emit(Event::Sgr(params));
                    }
                }
                j + 1
            } else {
                // Malformed or truncated: drop what we consumed, resume at the
                // offending byte so no text is swallowed.
                j
            }
        }
        b']' => match find_st(b, i + 1, true) {
            Some((body_end, after)) => {
                emit(Event::Osc(&text[i + 1..body_end]));
                after
            }
            // Unterminated: drop only the introducer rather than eating the
            // rest of the output.
            None => i + 1,
        },
        b'P' | b'X' | b'^' | b'_' => match find_st(b, i + 1, true) {
            Some((_, after)) => after,
            None => i + 1,
        },
        0x20..=0x2f => {
            // nF escape: intermediates then a final byte 0x30-0x7E.
            let mut j = i;
            while j < n && (0x20..=0x2f).contains(&b[j]) {
                j += 1;
            }
            if j < n && (0x30..=0x7e).contains(&b[j]) {
                j + 1
            } else {
                j
            }
        }
        0x30..=0x7e => i + 1, // two-byte escape (ESC 7, ESC =, ESC M, ESC c, …)
        _ => i,               // ESC followed by a control or non-ASCII: drop ESC
    }
}

/// Find the string terminator (BEL if `bel`, `ESC \`, or C1 ST U+009C) from
/// `from`. Returns (end of body, index after terminator).
fn find_st(b: &[u8], from: usize, bel: bool) -> Option<(usize, usize)> {
    let mut j = from;
    while let Some(k) = memchr::memchr3(BEL, ESC, 0xc2, &b[j..]) {
        let k = j + k;
        match b[k] {
            BEL if bel => return Some((k, k + 1)),
            ESC => {
                if b.get(k + 1) == Some(&b'\\') {
                    return Some((k, k + 2));
                }
                // ESC that is not ST aborts the string (xterm behaviour).
                return None;
            }
            0xc2 if b.get(k + 1) == Some(&0x9c) => return Some((k, k + 2)),
            _ => {}
        }
        j = k + 1;
    }
    None
}

/// Iterator over `;`-separated SGR parameters, each possibly with `:` sub-params.
struct Params<'a> {
    rest: Option<&'a str>,
}

impl<'a> Params<'a> {
    fn new(s: &'a str) -> Self {
        Params { rest: Some(s) }
    }
}

impl<'a> Iterator for Params<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        let s = self.rest?;
        match s.find(';') {
            Some(i) => {
                self.rest = Some(&s[i + 1..]);
                Some(&s[..i])
            }
            None => {
                self.rest = None;
                Some(s)
            }
        }
    }
}

/// Parse a decimal number; empty is 0 (`CSI ;1m` == `CSI 0;1m`). `None` for
/// anything that is not digits.
fn num(s: &str) -> Option<u32> {
    if s.is_empty() {
        return Some(0);
    }
    let mut v: u32 = 0;
    for &c in s.as_bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.saturating_mul(10).saturating_add((c - b'0') as u32);
    }
    Some(v)
}

fn byte(v: u32) -> Option<u8> {
    u8::try_from(v).ok()
}

/// Extended color from colon sub-parameters (`5:n`, `2::r:g:b`, `2:r:g:b`).
fn color_from_sub(sub: &str) -> Option<AnsiColor> {
    let mut it = sub.split(':');
    match num(it.next()?)? {
        5 => Some(AnsiColor::Indexed(byte(num(it.next()?)?)?)),
        2 => {
            let rest: Vec<&str> = it.collect();
            let rgb = match rest.len() {
                3 => &rest[..],
                n if n >= 4 => &rest[1..4], // colorspace id first (often empty)
                _ => return None,
            };
            Some(AnsiColor::Rgb(
                byte(num(rgb[0])?)?,
                byte(num(rgb[1])?)?,
                byte(num(rgb[2])?)?,
            ))
        }
        _ => None,
    }
}

/// Extended color from following `;` parameters (`5;n`, `2;r;g;b`).
fn color_from_params(params: &mut Params<'_>) -> Option<AnsiColor> {
    match num(params.next()?)? {
        5 => Some(AnsiColor::Indexed(byte(num(params.next()?)?)?)),
        2 => {
            let r = num(params.next()?)?;
            let g = num(params.next()?)?;
            let b = num(params.next()?)?;
            Some(AnsiColor::Rgb(byte(r)?, byte(g)?, byte(b)?))
        }
        _ => None,
    }
}

fn apply_sgr(st: &mut AnsiStyle, params: &str) {
    if params.is_empty() {
        *st = AnsiStyle::default();
        return;
    }
    let mut it = Params::new(params);
    while let Some(p) = it.next() {
        let (head, sub) = match p.find(':') {
            Some(i) => (&p[..i], Some(&p[i + 1..])),
            None => (p, None),
        };
        let Some(code) = num(head) else {
            // Garbage parameter: ignore it, keep going.
            continue;
        };
        match code {
            0 => *st = AnsiStyle::default(),
            1 => st.bold = true,
            2 => st.dim = true,
            3 => st.italic = true,
            4 => st.underline = !matches!(sub.map(num), Some(Some(0))),
            7 => st.inverse = true,
            9 => st.strikethrough = true,
            21 | 22 => {
                st.bold = false;
                st.dim = false;
            }
            23 => st.italic = false,
            24 => st.underline = false,
            27 => st.inverse = false,
            29 => st.strikethrough = false,
            30..=37 => st.fg = Some(AnsiColor::Named((code - 30) as u8)),
            39 => st.fg = None,
            40..=47 => st.bg = Some(AnsiColor::Named((code - 40) as u8)),
            49 => st.bg = None,
            90..=97 => st.fg = Some(AnsiColor::Named((code - 90 + 8) as u8)),
            100..=107 => st.bg = Some(AnsiColor::Named((code - 100 + 8) as u8)),
            38 | 48 | 58 => {
                let color = match sub {
                    Some(sub) => color_from_sub(sub),
                    None => color_from_params(&mut it),
                };
                match color {
                    Some(c) if code == 38 => st.fg = Some(c),
                    Some(c) if code == 48 => st.bg = Some(c),
                    Some(_) => {} // 58: underline color, not modelled
                    // Malformed extended color: JupyterLab drops the rest.
                    None => return,
                }
            }
            _ => {} // blink, conceal, fonts, overline, … : ignored
        }
    }
}

/// An sRGB color.
pub type Rgb = (u8, u8, u8);

/// RGB of an xterm 256-color index: 0–15 xterm's own system colors, 16–231
/// the 6×6×6 cube (`0, 95, 135, 175, 215, 255`), 232–255 the gray ramp
/// (`8 + 10·k`). Matches JupyterLab for 16–255; JupyterLab sends 0–15 to its
/// theme colors, which [`Palette::rgb`] does too.
pub fn xterm_256_rgb(idx: u8) -> Rgb {
    const SYSTEM: [Rgb; 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match idx {
        0..=15 => SYSTEM[idx as usize],
        16..=231 => {
            let i = idx - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

/// Theme colors for [`AnsiColor::Named`] (and indexed 0–15), plus the default
/// text and background colors used for inverse video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// `0..=7` normal (black, red, green, yellow, blue, magenta, cyan, white),
    /// `8..=15` "intense".
    pub colors: [Rgb; 16],
    /// Default text color (what unstyled text uses).
    pub foreground: Rgb,
    /// Default background (what inverse text uses as its color).
    pub background: Rgb,
}

impl Palette {
    /// JupyterLab's own ANSI colors (`@jupyterlab/rendermime` base.css). It uses
    /// them for both its light and dark themes; note its "intense" colors are
    /// darker than the normal ones.
    pub const JUPYTERLAB: Palette = Palette {
        colors: [
            (0x3e, 0x42, 0x4d),
            (0xe7, 0x5c, 0x58),
            (0x00, 0xa2, 0x50),
            (0xdd, 0xb6, 0x2b),
            (0x20, 0x8f, 0xfb),
            (0xd1, 0x60, 0xc4),
            (0x60, 0xc6, 0xc8),
            (0xc5, 0xc1, 0xb4),
            (0x28, 0x2c, 0x36),
            (0xb2, 0x2b, 0x31),
            (0x00, 0x74, 0x27),
            (0xb2, 0x7d, 0x12),
            (0x00, 0x65, 0xca),
            (0xa0, 0x31, 0x96),
            (0x25, 0x8f, 0x8f),
            (0xa1, 0xa6, 0xb2),
        ],
        foreground: (0x21, 0x21, 0x21),
        background: (0xff, 0xff, 0xff),
    };

    /// Light theme: JupyterLab's colors on white, so outputs look the way
    /// notebook users know them.
    pub const LIGHT: Palette = Palette::JUPYTERLAB;

    /// Dark theme: VS Code's dark terminal palette (bright variants really are
    /// brighter, all readable on a ~#1e1e1e background).
    pub const DARK: Palette = Palette {
        colors: [
            (0x00, 0x00, 0x00),
            (0xcd, 0x31, 0x31),
            (0x0d, 0xbc, 0x79),
            (0xe5, 0xe5, 0x10),
            (0x24, 0x72, 0xc8),
            (0xbc, 0x3f, 0xbc),
            (0x11, 0xa8, 0xcd),
            (0xe5, 0xe5, 0xe5),
            (0x66, 0x66, 0x66),
            (0xf1, 0x4c, 0x4c),
            (0x23, 0xd1, 0x8b),
            (0xf5, 0xf5, 0x43),
            (0x3b, 0x8e, 0xea),
            (0xd6, 0x70, 0xd6),
            (0x29, 0xb8, 0xdb),
            (0xe5, 0xe5, 0xe5),
        ],
        foreground: (0xcc, 0xcc, 0xcc),
        background: (0x1e, 0x1e, 0x1e),
    };

    /// Same colors with the theme's own default text/background colors.
    pub const fn with_defaults(mut self, foreground: Rgb, background: Rgb) -> Palette {
        self.foreground = foreground;
        self.background = background;
        self
    }

    /// RGB for a color: named and indexed 0–15 come from the palette, the rest
    /// from the xterm table.
    pub fn rgb(&self, c: AnsiColor) -> Rgb {
        match c {
            AnsiColor::Named(n) => self.colors[(n & 15) as usize],
            AnsiColor::Indexed(n) if n < 16 => self.colors[n as usize],
            AnsiColor::Indexed(n) => xterm_256_rgb(n),
            AnsiColor::Rgb(r, g, b) => (r, g, b),
        }
    }
}

/// A style with concrete colors, ready to paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedStyle {
    /// Text color (the palette foreground when unstyled).
    pub fg: Rgb,
    /// Background fill, `None` = transparent.
    pub bg: Option<Rgb>,
    pub bold: bool,
    /// Faint text; JupyterLab ignores it, apps usually lower the opacity.
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

impl AnsiStyle {
    /// Concrete colors following JupyterLab's `pushColoredChunk`: bold turns
    /// named colors 0–7 into their intense variant, inverse swaps foreground and
    /// background (defaults included), and yellow text on yellow background is
    /// drawn black (JupyterLab's traceback fix).
    pub fn resolve(&self, palette: &Palette) -> ResolvedStyle {
        let mut fg = self.fg;
        let mut bg = self.bg;
        if self.bold
            && let Some(AnsiColor::Named(n) | AnsiColor::Indexed(n)) = fg
            && n < 8
        {
            fg = Some(AnsiColor::Named(n + 8));
        }
        let yellow_on_yellow = fg == Some(AnsiColor::Named(3)) && bg == Some(AnsiColor::Named(3));
        if self.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        let fg_rgb = match fg {
            Some(c) => palette.rgb(c),
            None if self.inverse => palette.background,
            None => palette.foreground,
        };
        let bg_rgb = match bg {
            Some(c) => Some(palette.rgb(c)),
            None if self.inverse => Some(palette.foreground),
            None => None,
        };
        ResolvedStyle {
            fg: if yellow_on_yellow { (0, 0, 0) } else { fg_rgb },
            bg: bg_rgb,
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underline: self.underline,
            strikethrough: self.strikethrough,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(f: impl FnOnce(&mut AnsiStyle)) -> AnsiStyle {
        let mut s = AnsiStyle::default();
        f(&mut s);
        s
    }

    fn spans(t: &StyledText) -> Vec<(&str, AnsiStyle)> {
        t.spans
            .iter()
            .map(|s| (&t.text[s.range.clone()], s.style))
            .collect()
    }

    #[test]
    fn plain_text_untouched() {
        let t = parse("hello\nworld\t!");
        assert_eq!(t.text, "hello\nworld\t!");
        assert!(t.is_plain());
        assert!(matches!(strip("hello"), Cow::Borrowed(_)));
        assert!(!has_escapes("héllo © ü"));
    }

    #[test]
    fn basic_colors_and_reset() {
        let t = parse("\x1b[31mred\x1b[0m plain \x1b[1;32mbold green\x1b[m end");
        assert_eq!(t.text, "red plain bold green end");
        assert_eq!(
            spans(&t),
            vec![
                ("red", st(|s| s.fg = Some(AnsiColor::Named(1)))),
                (
                    "bold green",
                    st(|s| {
                        s.fg = Some(AnsiColor::Named(2));
                        s.bold = true
                    })
                ),
            ]
        );
    }

    #[test]
    fn ipython_traceback() {
        // As emitted by IPython 8 (ultratb).
        let tb = "\x1b[0;31m---------------------------------------------------------------------------\x1b[0m\n\x1b[0;31mZeroDivisionError\x1b[0m                         Traceback (most recent call last)\nCell \x1b[0;32mIn[1], line 1\x1b[0m\n\x1b[0;32m----> 1\x1b[0m \x1b[38;5;241m1\x1b[39m\x1b[38;5;241m/\x1b[39m\x1b[38;5;241m0\x1b[39m\n\n\x1b[0;31mZeroDivisionError\x1b[0m: division by zero";
        let t = parse(tb);
        assert!(!t.text.contains('\x1b'));
        assert!(t.text.ends_with("ZeroDivisionError: division by zero"));
        let one = t.text.find("1/0").unwrap();
        let s = t.spans.iter().find(|s| s.range.start == one).unwrap();
        // 1, /, 0 share a style -> merged into one span.
        assert_eq!(s.range, one..one + 3);
        assert_eq!(s.style.fg, Some(AnsiColor::Indexed(241)));
    }

    #[test]
    fn extended_colors_all_forms() {
        let t = parse(
            "\x1b[38;5;196ma\x1b[48;2;1;2;3mb\x1b[38:5:21mc\x1b[38:2::4:5:6md\x1b[38:2:7:8:9me\x1b[39;49mf",
        );
        assert_eq!(t.text, "abcdef");
        let s = &t.spans;
        assert_eq!(s[0].style.fg, Some(AnsiColor::Indexed(196)));
        assert_eq!(s[1].style.bg, Some(AnsiColor::Rgb(1, 2, 3)));
        assert_eq!(s[2].style.fg, Some(AnsiColor::Indexed(21)));
        assert_eq!(s[3].style.fg, Some(AnsiColor::Rgb(4, 5, 6)));
        assert_eq!(s[4].style.fg, Some(AnsiColor::Rgb(7, 8, 9)));
        assert_eq!(s.len(), 5); // f is default again
    }

    #[test]
    fn malformed_extended_color_drops_rest() {
        let t = parse("\x1b[38;5;1;1mx");
        assert_eq!(
            t.spans[0].style,
            st(|s| {
                s.fg = Some(AnsiColor::Indexed(1));
                s.bold = true
            })
        );
        let t = parse("\x1b[38;2;300;0;0;1mx");
        assert!(
            t.is_plain(),
            "out-of-range rgb drops the rest like JupyterLab"
        );
        let t = parse("\x1b[1;38;5mx");
        assert_eq!(t.spans[0].style, st(|s| s.bold = true));
    }

    #[test]
    fn attributes_and_resets() {
        let t = parse("\x1b[1;2;3;4;7;9ma\x1b[22;23;24;27;29mb\x1b[4:0mc\x1b[4:3md\x1b[21me");
        assert_eq!(t.text, "abcde");
        assert_eq!(
            t.spans[0].style,
            st(|s| {
                s.bold = true;
                s.dim = true;
                s.italic = true;
                s.underline = true;
                s.inverse = true;
                s.strikethrough = true
            })
        );
        assert_eq!(t.spans.len(), 2);
        assert_eq!(&t.text[t.spans[1].range.clone()], "de");
        assert!(t.spans[1].style.underline);
    }

    #[test]
    fn bright_and_bg() {
        let t = parse("\x1b[91;104mx\x1b[49my");
        assert_eq!(t.spans[0].style.fg, Some(AnsiColor::Named(9)));
        assert_eq!(t.spans[0].style.bg, Some(AnsiColor::Named(12)));
        assert_eq!(t.spans[1].style.bg, None);
    }

    #[test]
    fn empty_params_mean_zero() {
        let t = parse("\x1b[1m\x1b[;31mx");
        assert_eq!(t.spans[0].style, st(|s| s.fg = Some(AnsiColor::Named(1))));
    }

    #[test]
    fn strips_other_sequences() {
        let s = "a\x1b[2Kb\x1b[?25lc\x1b[1;1Hd\x1b]0;title\x07e\x1b]2;t\x1b\\f\x1b(Bg\x1b7h\x1bPdcs\x1b\\i\x1b[>4;2mj\x1b";
        assert_eq!(parse(s).text, "abcdefghij");
        assert!(parse(s).is_plain());
        assert_eq!(strip(s), "abcdefghij");
    }

    #[test]
    fn controls_removed_tab_newline_kept() {
        assert_eq!(
            parse("a\x07b\x00c\rd\x08e\x7ff\tg\nh\u{9b}i\u{85}j").text,
            "abcdef\tg\nhij"
        );
        assert_eq!(parse("©é").text, "©é");
    }

    #[test]
    fn malformed_csi_does_not_eat_text() {
        assert_eq!(parse("\x1b[31\nnext").text, "\nnext");
        assert_eq!(parse("x\x1b[").text, "x");
        assert_eq!(
            parse("\x1b]unterminated osc rest").text,
            "unterminated osc rest"
        );
    }

    #[test]
    fn osc8_links() {
        let t = parse(
            "see \x1b]8;;https://example.com\x1b\\docs\x1b]8;;\x1b\\ and \x1b]8;id=1;http://a.b\x07here",
        );
        assert_eq!(t.text, "see docs and here");
        assert_eq!(t.links.len(), 2);
        assert_eq!(&t.text[t.links[0].range.clone()], "docs");
        assert_eq!(t.links[0].url, "https://example.com");
        assert_eq!(&t.text[t.links[1].range.clone()], "here");
        assert_eq!(t.links[1].url, "http://a.b");
    }

    #[test]
    fn runs_cover_everything() {
        let t = parse("a\x1b[31mb\x1b[0mc\x1b[32md");
        let runs: Vec<_> = t.runs().map(|(r, s)| (&t.text[r], s.fg)).collect();
        assert_eq!(
            runs,
            vec![
                ("a", None),
                ("b", Some(AnsiColor::Named(1))),
                ("c", None),
                ("d", Some(AnsiColor::Named(2)))
            ]
        );
        assert_eq!(parse("").runs().count(), 0);
    }

    #[test]
    fn xterm_table() {
        assert_eq!(xterm_256_rgb(16), (0, 0, 0));
        assert_eq!(xterm_256_rgb(196), (255, 0, 0));
        assert_eq!(xterm_256_rgb(241), (98, 98, 98));
        assert_eq!(xterm_256_rgb(231), (255, 255, 255));
        assert_eq!(xterm_256_rgb(232), (8, 8, 8));
        assert_eq!(xterm_256_rgb(255), (238, 238, 238));
        // JupyterLab's cube formula for comparison.
        for idx in 16u32..232 {
            let r = (idx - 16) / 36;
            let r = if r > 0 { 55 + r * 40 } else { 0 };
            assert_eq!(xterm_256_rgb(idx as u8).0 as u32, r);
        }
    }

    #[test]
    fn resolve_like_jupyterlab() {
        let p = Palette::LIGHT;
        let bold_green = st(|s| {
            s.fg = Some(AnsiColor::Named(2));
            s.bold = true
        });
        assert_eq!(bold_green.resolve(&p).fg, p.colors[10]);
        let inv = st(|s| s.inverse = true).resolve(&p);
        assert_eq!(inv.fg, p.background);
        assert_eq!(inv.bg, Some(p.foreground));
        let yy = st(|s| {
            s.fg = Some(AnsiColor::Named(3));
            s.bg = Some(AnsiColor::Named(3))
        });
        assert_eq!(yy.resolve(&p).fg, (0, 0, 0));
        assert_eq!(
            st(|s| s.fg = Some(AnsiColor::Indexed(3)))
                .resolve(&Palette::DARK)
                .fg,
            Palette::DARK.colors[3]
        );
    }

    #[test]
    fn utf8_boundaries_safe() {
        let s = "é\x1b[31m日本\x1b[0m😀\u{9c}x";
        let t = parse(s);
        assert_eq!(t.text, "é日本😀x");
        for sp in &t.spans {
            assert!(
                t.text.is_char_boundary(sp.range.start) && t.text.is_char_boundary(sp.range.end)
            );
        }
    }
}
