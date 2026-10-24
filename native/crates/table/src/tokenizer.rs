//! A small, allocation-free HTML tokenizer.
//!
//! It follows the WHATWG tokenizer where it matters for notebook outputs —
//! quoted attribute values may contain `>`, `<style>` / `<script>` /
//! `<textarea>` / `<title>` contents are raw text, comments, `<!DOCTYPE>`,
//! processing instructions and CDATA are skipped — and is lenient everywhere
//! else. Every slice it hands out starts and ends at an ASCII byte, so slicing
//! the `&str` can never split a UTF-8 sequence; all indexing goes through
//! `get` anyway, so garbage input cannot panic.

use memchr::{memchr, memchr2};

/// Element names the table builder cares about. Everything else is `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tag {
    Table,
    Caption,
    Thead,
    Tbody,
    Tfoot,
    Tr,
    Th,
    Td,
    Colgroup,
    Col,
    Style,
    Script,
    /// Raw-text elements whose content is never rendered as markup and that
    /// a table output never needs: textarea, title, xmp, iframe, noembed,
    /// noframes, noscript.
    RawOther,
    /// `<plaintext>`: the rest of the document is text.
    Plaintext,
    Br,
    Wbr,
    B,
    Strong,
    I,
    Em,
    /// Block-level elements that start a new line inside a cell.
    Block,
    Pre,
    P,
    Div,
    Small,
    Span,
    Html,
    Head,
    Body,
    Meta,
    Link,
    Base,
    Hr,
    /// Inline formatting with no effect on the model (a, code, font, sub, ...).
    Inline,
    /// Embedded or interactive content a native grid cannot reproduce.
    Media,
    Svg,
    Math,
    Button,
    Template,
    Other,
}

impl Tag {
    /// Map a tag name (any case) to a [`Tag`].
    #[inline]
    pub(crate) fn from_name(name: &[u8]) -> Tag {
        // Fast path for the tags a DataFrame is made of.
        if name.len() == 2 && name[0] | 0x20 == b't' {
            match name[1] | 0x20 {
                b'd' => return Tag::Td,
                b'h' => return Tag::Th,
                b'r' => return Tag::Tr,
                _ => {}
            }
        }
        if name.len() > 10 {
            return Tag::Other;
        }
        let mut buf = [0u8; 10];
        for (d, s) in buf.iter_mut().zip(name) {
            *d = s.to_ascii_lowercase();
        }
        match &buf[..name.len()] {
            b"table" => Tag::Table,
            b"caption" => Tag::Caption,
            b"thead" => Tag::Thead,
            b"tbody" => Tag::Tbody,
            b"tfoot" => Tag::Tfoot,
            b"tr" => Tag::Tr,
            b"th" => Tag::Th,
            b"td" => Tag::Td,
            b"colgroup" => Tag::Colgroup,
            b"col" => Tag::Col,
            b"style" => Tag::Style,
            b"script" => Tag::Script,
            b"textarea" | b"title" | b"xmp" | b"iframe" | b"noembed" | b"noframes" | b"noscript" => {
                Tag::RawOther
            }
            b"plaintext" => Tag::Plaintext,
            b"br" => Tag::Br,
            b"wbr" => Tag::Wbr,
            b"b" => Tag::B,
            b"strong" => Tag::Strong,
            b"i" => Tag::I,
            b"em" => Tag::Em,
            b"pre" | b"listing" => Tag::Pre,
            b"p" => Tag::P,
            b"div" => Tag::Div,
            b"small" => Tag::Small,
            b"span" => Tag::Span,
            b"html" => Tag::Html,
            b"head" => Tag::Head,
            b"body" => Tag::Body,
            b"meta" => Tag::Meta,
            b"link" => Tag::Link,
            b"base" => Tag::Base,
            b"hr" => Tag::Hr,
            b"ul" | b"ol" | b"li" | b"dl" | b"dt" | b"dd" | b"h1" | b"h2" | b"h3" | b"h4" | b"h5"
            | b"h6" | b"blockquote" | b"section" | b"article" | b"header" | b"footer" | b"main"
            | b"nav" | b"aside" | b"figure" | b"figcaption" | b"address" | b"center" | b"fieldset"
            | b"legend" | b"form" | b"details" | b"summary" => Tag::Block,
            b"a" | b"code" | b"tt" | b"kbd" | b"samp" | b"var" | b"font" | b"sub" | b"sup" | b"u"
            | b"s" | b"strike" | b"del" | b"ins" | b"mark" | b"abbr" | b"cite" | b"q" | b"dfn"
            | b"time" | b"label" | b"big" | b"nobr" | b"bdi" | b"bdo" | b"data" | b"output" => {
                Tag::Inline
            }
            b"img" | b"video" | b"audio" | b"canvas" | b"object" | b"embed" | b"picture"
            | b"source" | b"track" | b"input" | b"select" | b"option" | b"progress" | b"meter"
            | b"map" | b"area" | b"frame" | b"frameset" | b"applet" | b"dialog" | b"slot" => {
                Tag::Media
            }
            b"svg" => Tag::Svg,
            b"math" => Tag::Math,
            b"button" => Tag::Button,
            b"template" => Tag::Template,
            _ => Tag::Other,
        }
    }

    /// HTML void elements: never have content or an end tag.
    pub(crate) fn is_void(self, name: &[u8]) -> bool {
        match self {
            Tag::Br | Tag::Wbr | Tag::Col | Tag::Meta | Tag::Link | Tag::Base | Tag::Hr => true,
            Tag::Media => {
                let n = |s: &[u8]| name.eq_ignore_ascii_case(s);
                n(b"img") || n(b"embed") || n(b"source") || n(b"track") || n(b"input") || n(b"area")
            }
            Tag::Other => {
                let n = |s: &[u8]| name.eq_ignore_ascii_case(s);
                n(b"keygen") || n(b"param")
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Token<'a> {
    Start {
        tag: Tag,
        name: &'a str,
        /// The raw attribute text between the name and `>`; iterate it with
        /// [`Attrs`].
        attrs: &'a str,
        self_closing: bool,
    },
    End {
        tag: Tag,
        name: &'a str,
    },
    /// Character data (entities not yet decoded).
    Text(&'a str),
    /// Contents of a raw-text element (`<style>`, `<script>`, ...).
    Raw { tag: Tag, text: &'a str },
}

pub(crate) struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
    /// After a raw-text start tag: the element whose end tag closes the text.
    raw: Option<(Tag, &'a str)>,
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

impl<'a> Tokenizer<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Tokenizer { src, pos: 0, raw: None }
    }

    #[inline]
    fn slice(&self, a: usize, b: usize) -> &'a str {
        self.src.get(a..b).unwrap_or("")
    }

    /// Find `</name` (case-insensitive) followed by whitespace, `/`, `>` or EOF.
    fn find_close(&self, from: usize, name: &str) -> usize {
        let b = self.src.as_bytes();
        let n = name.as_bytes();
        let mut i = from;
        while let Some(off) = b.get(i..).and_then(|s| memchr(b'<', s)) {
            let lt = i + off;
            if b.get(lt + 1) == Some(&b'/')
                && b.get(lt + 2..lt + 2 + n.len()).is_some_and(|s| s.eq_ignore_ascii_case(n))
                && b.get(lt + 2 + n.len()).is_none_or(|&c| is_ws(c) || c == b'/' || c == b'>')
            {
                return lt;
            }
            i = lt + 1;
        }
        b.len()
    }

    /// Skip to just past the next `>` (bogus comments, `<!DOCTYPE>`, `<?..>`).
    fn skip_past_gt(&mut self, from: usize) {
        let b = self.src.as_bytes();
        self.pos = match b.get(from..).and_then(|s| memchr(b'>', s)) {
            Some(off) => from + off + 1,
            None => b.len(),
        };
    }

    fn skip_past(&mut self, from: usize, needle: &[u8]) {
        let b = self.src.as_bytes();
        self.pos = match b.get(from..).and_then(|s| memchr::memmem::find(s, needle)) {
            Some(off) => from + off + needle.len(),
            None => b.len(),
        };
    }

    /// Parse `<name attrs>` starting at `lt` (which points at `<`). Returns
    /// the name range and the index of the closing `>` (or `None` at EOF).
    fn scan_tag(&self, name_start: usize) -> (usize, Option<usize>) {
        let b = self.src.as_bytes();
        let mut i = name_start;
        while i < b.len() && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' {
            i += 1;
        }
        let name_end = i;
        if b.get(i) == Some(&b'>') {
            return (name_end, Some(i));
        }
        let mut attrs = Attrs { s: self.src, i: name_end };
        while attrs.next().is_some() {}
        let gt = if attrs.i < b.len() && b[attrs.i] == b'>' { Some(attrs.i) } else { None };
        (name_end, gt)
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let b = self.src.as_bytes();
        if self.raw.is_some()
            && let Some((tag, name)) = self.raw.take()
        {
            let start = self.pos;
            let end = if tag == Tag::Plaintext { b.len() } else { self.find_close(start, name) };
            self.pos = end;
            return Some(Token::Raw { tag, text: self.slice(start, end) });
        }
        loop {
            let pos = self.pos;
            if pos >= b.len() {
                return None;
            }
            if b[pos] != b'<' {
                let end = match memchr(b'<', &b[pos..]) {
                    Some(off) => pos + off,
                    None => b.len(),
                };
                self.pos = end;
                return Some(Token::Text(self.slice(pos, end)));
            }
            match b.get(pos + 1).copied() {
                Some(c) if c.is_ascii_alphabetic() => {
                    let (name_end, gt) = self.scan_tag(pos + 1);
                    let Some(gt) = gt else {
                        // EOF inside a tag: the tag is dropped.
                        self.pos = b.len();
                        return None;
                    };
                    self.pos = gt + 1;
                    let name = self.slice(pos + 1, name_end);
                    let tag = Tag::from_name(name.as_bytes());
                    let attrs = self.slice(name_end, gt);
                    let self_closing = gt > name_end && b[gt - 1] == b'/';
                    match tag {
                        Tag::Style | Tag::Script | Tag::RawOther | Tag::Plaintext => {
                            self.raw = Some((tag, name));
                        }
                        _ => {}
                    }
                    return Some(Token::Start { tag, name, attrs, self_closing });
                }
                Some(b'/') => match b.get(pos + 2).copied() {
                    Some(c) if c.is_ascii_alphabetic() => {
                        let (name_end, gt) = self.scan_tag(pos + 2);
                        let Some(gt) = gt else {
                            self.pos = b.len();
                            return None;
                        };
                        self.pos = gt + 1;
                        let name = self.slice(pos + 2, name_end);
                        return Some(Token::End { tag: Tag::from_name(name.as_bytes()), name });
                    }
                    Some(b'>') => self.pos = pos + 3,
                    None => {
                        self.pos = b.len();
                        return Some(Token::Text(self.slice(pos, b.len())));
                    }
                    _ => self.skip_past_gt(pos + 2),
                },
                Some(b'!') => {
                    let rest = &b[pos..];
                    if rest.starts_with(b"<!--") {
                        // `<!-->` and `<!--->` are (empty) complete comments.
                        if rest.get(4) == Some(&b'>') {
                            self.pos = pos + 5;
                        } else if rest.get(4..6) == Some(b"->") {
                            self.pos = pos + 6;
                        } else {
                            self.skip_past(pos + 4, b"-->");
                        }
                    } else if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case(b"<![CDATA[") {
                        self.skip_past(pos + 9, b"]]>");
                    } else {
                        self.skip_past_gt(pos + 2);
                    }
                }
                Some(b'?') => self.skip_past_gt(pos + 2),
                _ => {
                    // A lone `<`: literal text up to the next `<`.
                    let end = match memchr(b'<', &b[pos + 1..]) {
                        Some(off) => pos + 1 + off,
                        None => b.len(),
                    };
                    self.pos = end;
                    return Some(Token::Text(self.slice(pos, end)));
                }
            }
        }
    }
}

/// Iterator over `(name, raw_value)` attribute pairs of a start tag's
/// attribute text. Values are not entity-decoded (see [`crate::entities`]).
/// Stops at `>` or the end of the input; `i` is left pointing there.
pub(crate) struct Attrs<'a> {
    s: &'a str,
    pub(crate) i: usize,
}

impl<'a> Attrs<'a> {
    pub(crate) fn new(s: &'a str) -> Self {
        Attrs { s, i: 0 }
    }
}

impl<'a> Iterator for Attrs<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<(&'a str, &'a str)> {
        let b = self.s.as_bytes();
        let mut i = self.i;
        while i < b.len() && (is_ws(b[i]) || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() || b[i] == b'>' {
            self.i = i;
            return None;
        }
        // The first character of a name may be anything (even `=`).
        let name_start = i;
        i += 1;
        while i < b.len() && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' && b[i] != b'=' {
            i += 1;
        }
        let name_end = i;
        let mut j = i;
        while j < b.len() && is_ws(b[j]) {
            j += 1;
        }
        let mut value = (0, 0);
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && is_ws(b[j]) {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                match memchr(q, &b[j + 1..]) {
                    Some(off) => {
                        value = (j + 1, j + 1 + off);
                        i = j + 2 + off;
                    }
                    None => {
                        // EOF inside a quoted value.
                        self.i = b.len();
                        return None;
                    }
                }
            } else {
                let v0 = j;
                while j < b.len() && !is_ws(b[j]) && b[j] != b'>' {
                    j += 1;
                }
                value = (v0, j);
                i = j;
            }
        }
        self.i = i;
        let get = |a: usize, z: usize| self.s.get(a..z).unwrap_or("");
        Some((get(name_start, name_end), get(value.0, value.1)))
    }
}

/// Case-insensitive search for an ASCII needle (used for cheap pre-checks).
pub(crate) fn find_ascii_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let (first, rest) = needle.split_first()?;
    let (lo, up) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
    let mut i = 0;
    while let Some(off) = hay.get(i..).and_then(|s| memchr2(lo, up, s)) {
        let at = i + off;
        if hay.get(at + 1..at + 1 + rest.len()).is_some_and(|s| s.eq_ignore_ascii_case(rest)) {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        Tokenizer::new(s)
            .map(|t| match t {
                Token::Start { name, attrs, self_closing, .. } => {
                    format!("<{name}{attrs}{}>", if self_closing { "/" } else { "" })
                }
                Token::End { name, .. } => format!("</{name}>"),
                Token::Text(t) => format!("T({t})"),
                Token::Raw { text, .. } => format!("R({text})"),
            })
            .collect()
    }

    #[test]
    fn basic() {
        assert_eq!(toks("<td a='x>y'>1</td>"), ["<td a='x>y'>", "T(1)", "</td>"]);
        assert_eq!(toks("a < b <!-- c --> d"), ["T(a )", "T(< b )", "T( d)"]);
        assert_eq!(toks("<style>td{}</td></STYLE >x"), ["<style>", "R(td{}</td>)", "</STYLE>", "T(x)"]);
        assert_eq!(toks("<!DOCTYPE html><p>"), ["<p>"]);
        assert_eq!(toks("<td"), Vec::<String>::new());
        assert_eq!(toks("<td a=\"unterminated>x"), Vec::<String>::new());
        assert_eq!(toks("</>x</"), ["T(x)", "T(</)"]);
    }

    #[test]
    fn attrs() {
        let a: Vec<_> = Attrs::new(r#" class = "a b"  colspan=2 x checked id='q'"#).collect();
        assert_eq!(a, [("class", "a b"), ("colspan", "2"), ("x", ""), ("checked", ""), ("id", "q")]);
    }

    #[test]
    fn find_ci() {
        assert_eq!(find_ascii_ci(b"xx<TaBle", b"<table"), Some(2));
        assert_eq!(find_ascii_ci(b"<tabl", b"<table"), None);
    }
}
