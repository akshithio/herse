//! Character references and browser-style whitespace collapsing.

use std::borrow::Cow;

#[path = "entities_table.rs"]
mod entities_table;
use entities_table::ENTITIES;

/// Decode HTML character references (`&amp;`, `&nbsp;`, `&#215;`, `&#x2026;`,
/// legacy `&amp` without `;`) the way a browser does in text content.
pub(crate) fn decode(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().contains(&b'&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for_each_piece(s, |p| out.push_str(p));
    Cow::Owned(out)
}

/// Call `f` with the decoded text of `s` in pieces (literal runs and
/// replacement characters), without allocating.
fn for_each_piece(s: &str, mut f: impl FnMut(&str)) {
    let mut rest = s;
    let mut utf8 = [0u8; 4];
    while let Some(i) = memchr::memchr(b'&', rest.as_bytes()) {
        if i > 0 {
            f(&rest[..i]);
        }
        let after = &rest[i + 1..];
        match decode_one(after) {
            Some((Decoded::Char(c), used)) => {
                f(c.encode_utf8(&mut utf8));
                rest = after.get(used..).unwrap_or("");
            }
            Some((Decoded::Str(t), used)) => {
                f(t);
                rest = after.get(used..).unwrap_or("");
            }
            None => {
                f("&");
                rest = after;
            }
        }
    }
    if !rest.is_empty() {
        f(rest);
    }
}

enum Decoded {
    Char(char),
    Str(&'static str),
}

/// `s` starts right after the `&`. Returns the replacement and how many bytes
/// of `s` it consumed.
fn decode_one(s: &str) -> Option<(Decoded, usize)> {
    let b = s.as_bytes();
    if b.first() == Some(&b'#') {
        let hex = matches!(b.get(1), Some(b'x' | b'X'));
        let mut i = if hex { 2 } else { 1 };
        let start = i;
        let mut v: u32 = 0;
        while let Some(&c) = b.get(i) {
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' if hex => (c - b'a' + 10) as u32,
                b'A'..=b'F' if hex => (c - b'A' + 10) as u32,
                _ => break,
            };
            v = v.saturating_mul(if hex { 16 } else { 10 }).saturating_add(d).min(0x11_0000);
            i += 1;
        }
        if i == start {
            return None;
        }
        if b.get(i) == Some(&b';') {
            i += 1;
        }
        return Some((Decoded::Char(numeric_char(v)), i));
    }
    let n = b.iter().take(40).take_while(|c| c.is_ascii_alphanumeric()).count();
    if n == 0 {
        return None;
    }
    if b.get(n) == Some(&b';') {
        // The references DataFrame HTML is full of, without the table search.
        let common = match &s[..n] {
            "quot" => Some("\""),
            "amp" => Some("&"),
            "lt" => Some("<"),
            "gt" => Some(">"),
            "nbsp" => Some("\u{a0}"),
            "apos" => Some("'"),
            "hellip" => Some("…"),
            "times" => Some("×"),
            _ => None,
        };
        if let Some(v) = common.or_else(|| lookup(&s[..=n])) {
            return Some((Decoded::Str(v), n + 1));
        }
    }
    // Legacy references that browsers accept without the `;` (longest match).
    for k in (2..=n.min(8)).rev() {
        if let Some(v) = lookup(&s[..k]) {
            return Some((Decoded::Str(v), k));
        }
    }
    None
}

fn lookup(name: &str) -> Option<&'static str> {
    ENTITIES
        .binary_search_by(|(k, _)| k.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| ENTITIES[i].1)
}

/// Numeric references per the HTML spec (NUL / surrogates / out of range ->
/// U+FFFD, C1 controls remapped through windows-1252).
fn numeric_char(v: u32) -> char {
    const W1252: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
        0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
        0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    let v = if (0x80..0xA0).contains(&v) { W1252[(v - 0x80) as usize] as u32 } else { v };
    if v == 0 {
        return '\u{FFFD}';
    }
    char::from_u32(v).unwrap_or('\u{FFFD}')
}

#[inline]
pub(crate) fn is_html_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// Accumulates the rendered text of a cell (or caption / footer): entities
/// decoded, runs of whitespace collapsed to one space, `<br>` kept as `\n`,
/// `&nbsp;` turned into a plain space.
#[derive(Default)]
pub(crate) struct TextBuf {
    pub(crate) s: String,
    pending_space: bool,
    /// `white-space: pre` / `<pre>`: keep whitespace as written.
    pub(crate) pre: bool,
}

impl TextBuf {
    pub(crate) fn with_pre(pre: bool) -> Self {
        TextBuf { pre, ..Default::default() }
    }

    pub(crate) fn push_html(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        if self.s.capacity() == 0 {
            // Most cells are one text token: allocate once, exactly. Decoded
            // text is never longer than its source.
            self.s.reserve_exact(raw.len());
        }
        if self.pre {
            for_each_piece(raw, |p| self.push_piece(p));
            return;
        }
        if raw.bytes().all(|b| b != b'&' && !is_html_ws(b) && b != 0xC2) {
            self.flush_space();
            self.s.push_str(raw);
            return;
        }
        for_each_piece(raw, |p| self.push_piece(p));
    }

    /// Decoded text: runs without whitespace or NBSP go in whole.
    fn push_piece(&mut self, p: &str) {
        let plain = p.bytes().all(|b| !is_html_ws(b) && b != 0xC2);
        if plain {
            if !self.pre {
                self.flush_space();
            }
            self.s.push_str(p);
        } else {
            for c in p.chars() {
                self.push_char(c);
            }
        }
    }

    #[inline]
    fn flush_space(&mut self) {
        if self.pending_space {
            if !self.s.is_empty() && !self.s.ends_with('\n') {
                self.s.push(' ');
            }
            self.pending_space = false;
        }
    }

    fn push_char(&mut self, c: char) {
        if self.pre {
            match c {
                '\r' => {}
                '\u{a0}' => self.s.push(' '),
                _ => self.s.push(c),
            }
            return;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' | '\x0c' => self.pending_space = true,
            '\u{a0}' => {
                self.flush_space();
                self.s.push(' ');
            }
            _ => {
                self.flush_space();
                self.s.push(c);
            }
        }
    }

    /// `<br>`.
    pub(crate) fn newline(&mut self) {
        self.s.push('\n');
        self.pending_space = false;
    }

    /// Start or end of a block element (`<p>`, `<div>`, `<li>`, ...).
    pub(crate) fn block_break(&mut self) {
        if !self.s.is_empty() && !self.s.ends_with('\n') {
            self.s.push('\n');
        }
        self.pending_space = false;
    }

    pub(crate) fn finish(mut self) -> String {
        let b = self.s.as_bytes();
        let end = b.len() - b.iter().rev().take_while(|&&c| c == b' ' || c == b'\n').count();
        self.s.truncate(end);
        let start = self.s.bytes().take_while(|&c| c == b' ' || c == b'\n').count();
        if start > 0 {
            self.s.drain(..start);
        }
        self.s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities() {
        assert_eq!(decode("a &amp; b &lt;c&gt; &quot;"), "a & b <c> \"");
        assert_eq!(decode("&nbsp;&hellip;&times;&#215;&#xD7;&#x2026"), "\u{a0}…×××…");
        assert_eq!(decode("&amp &ampx &notit; &copy2024"), "& &x ¬it; ©2024");
        assert_eq!(decode("&#0; &#x110000; &#xD800; &#150; &#"), "\u{FFFD} \u{FFFD} \u{FFFD} – &#");
        assert_eq!(decode("&unknown; & &;"), "&unknown; & &;");
        assert_eq!(decode("&NotNestedGreaterGreater; &fjlig;"), "⪢\u{338} fj");
    }

    #[test]
    fn collapse() {
        let mut t = TextBuf::default();
        t.push_html("\n   hello \t  world  ");
        t.newline();
        t.push_html("  next&nbsp;&nbsp;line ");
        assert_eq!(t.finish(), "hello world\nnext  line");
        let mut t = TextBuf::default();
        t.push_html("&nbsp;");
        assert_eq!(t.finish(), "");
    }
}
