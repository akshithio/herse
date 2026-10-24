//! A tiny, allocation-free, byte-level JSON scanner specialised for `.ipynb`.
//!
//! Notebooks are dominated by output payloads (base64 images, long text
//! streams, widget state) that the workspace never needs. A general parser
//! still has to validate and copy every byte of those strings; this scanner
//! skips a string with a single `memchr` for the closing quote (checking the
//! run of backslashes before it), and skips containers with a depth counter.
//! Only the handful of fields we care about are decoded.
//!
//! The scanner is lenient: it does not validate the grammar of values it
//! skips (numbers, mismatched bracket kinds, invalid UTF-8 inside skipped
//! strings). For valid JSON it is exact; for invalid JSON it either errors or
//! returns a best-effort answer. Callers fall back to `serde_json` on error.

use memchr::memchr;
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanError {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for ScanError {}

pub(crate) type R<T> = Result<T, ScanError>;

/// Byte classes for container skipping: 0 = uninteresting.
static CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    t[b'"' as usize] = 1;
    t[b'[' as usize] = 2;
    t[b'{' as usize] = 2;
    t[b']' as usize] = 3;
    t[b'}' as usize] = 3;
    t
};

pub(crate) struct Cursor<'a> {
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        let i = if b.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
        Cursor { b, i }
    }

    #[inline]
    fn err<T>(&self, what: &'static str) -> R<T> {
        Err(ScanError { at: self.i, what })
    }

    #[inline]
    pub fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if c == b' ' || c == b'\n' || c == b'\r' || c == b'\t' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// Next non-whitespace byte (not consumed).
    #[inline]
    pub fn peek(&mut self) -> R<u8> {
        self.ws();
        match self.b.get(self.i) {
            Some(&c) => Ok(c),
            None => self.err("unexpected end of input"),
        }
    }

    #[inline]
    pub fn expect(&mut self, c: u8) -> R<()> {
        if self.peek()? == c {
            self.i += 1;
            Ok(())
        } else {
            self.err(match c {
                b'{' => "expected '{'",
                b'[' => "expected '['",
                b':' => "expected ':'",
                b'"' => "expected string",
                _ => "unexpected character",
            })
        }
    }

    /// Cursor must be at an opening quote. Returns the raw (still escaped)
    /// contents and leaves the cursor after the closing quote.
    #[inline]
    pub fn string_raw(&mut self) -> R<&'a [u8]> {
        debug_assert_eq!(self.b.get(self.i), Some(&b'"'));
        let b = self.b;
        let start = self.i + 1;
        let mut pos = start;
        loop {
            let Some(off) = memchr(b'"', &b[pos.min(b.len())..]) else {
                self.i = b.len();
                return self.err("unterminated string");
            };
            let q = pos + off;
            let mut k = 0;
            while q - k > start && b[q - k - 1] == b'\\' {
                k += 1;
            }
            if k % 2 == 0 {
                self.i = q + 1;
                return Ok(&b[start..q]);
            }
            pos = q + 1;
        }
    }

    /// Skips any JSON value.
    pub fn skip_value(&mut self) -> R<()> {
        match self.peek()? {
            b'"' => self.string_raw().map(|_| ()),
            b'{' | b'[' => self.skip_container(),
            _ => self.skip_scalar(),
        }
    }

    /// Skips any JSON value and returns its raw bytes.
    pub fn raw_value(&mut self) -> R<&'a [u8]> {
        self.ws();
        let start = self.i;
        self.skip_value()?;
        Ok(&self.b[start..self.i])
    }

    fn skip_scalar(&mut self) -> R<()> {
        let start = self.i;
        while let Some(&c) = self.b.get(self.i) {
            if matches!(c, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t') {
                break;
            }
            self.i += 1;
        }
        if self.i == start {
            self.err("expected value")
        } else {
            Ok(())
        }
    }

    fn skip_container(&mut self) -> R<()> {
        let b = self.b;
        let mut i = self.i;
        let mut depth: usize = 0;
        while i < b.len() {
            match CLASS[b[i] as usize] {
                0 => {}
                1 => {
                    self.i = i;
                    self.string_raw()?;
                    i = self.i;
                    continue;
                }
                2 => depth += 1,
                _ => {
                    depth -= 1;
                    if depth == 0 {
                        self.i = i + 1;
                        return Ok(());
                    }
                }
            }
            i += 1;
        }
        self.i = b.len();
        self.err("unterminated container")
    }

    /// Iterates the members of an object. `f` receives the (unescaped) key
    /// with the cursor positioned at the value and must consume the value.
    pub fn object(&mut self, mut f: impl FnMut(&mut Self, Cow<'a, [u8]>) -> R<()>) -> R<()> {
        self.expect(b'{')?;
        if self.peek()? == b'}' {
            self.i += 1;
            return Ok(());
        }
        loop {
            if self.peek()? != b'"' {
                return self.err("expected object key");
            }
            let raw = self.string_raw()?;
            let key = if memchr(b'\\', raw).is_some() {
                let mut s = String::new();
                decode_str(raw, &mut s);
                Cow::Owned(s.into_bytes())
            } else {
                Cow::Borrowed(raw)
            };
            self.expect(b':')?;
            f(self, key)?;
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return self.err("expected ',' or '}'"),
            }
        }
    }

    /// Iterates the elements of an array; `f` gets the element index and
    /// must consume the element.
    pub fn array(&mut self, mut f: impl FnMut(&mut Self, usize) -> R<()>) -> R<()> {
        self.expect(b'[')?;
        if self.peek()? == b']' {
            self.i += 1;
            return Ok(());
        }
        let mut n = 0;
        loop {
            f(self, n)?;
            n += 1;
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return self.err("expected ',' or ']'"),
            }
        }
    }

    /// If the value is a string, decodes it; otherwise skips it.
    pub fn opt_string(&mut self) -> R<Option<String>> {
        if self.peek()? == b'"' {
            let raw = self.string_raw()?;
            let mut s = String::with_capacity(raw.len());
            decode_str(raw, &mut s);
            Ok(Some(s))
        } else {
            self.skip_value()?;
            Ok(None)
        }
    }

    /// If the value is a string, returns its raw (escaped) bytes; otherwise skips it.
    pub fn opt_string_raw(&mut self) -> R<Option<&'a [u8]>> {
        if self.peek()? == b'"' {
            Ok(Some(self.string_raw()?))
        } else {
            self.skip_value()?;
            Ok(None)
        }
    }

    /// Parses a non-negative integer, or skips a non-integer value.
    pub fn opt_u32(&mut self) -> R<Option<u32>> {
        self.ws();
        let start = self.i;
        self.skip_value()?;
        let raw = &self.b[start..self.i];
        Ok(std::str::from_utf8(raw).ok().and_then(|s| s.parse().ok()))
    }

    /// For an array value: returns whether it has at least one element, and
    /// skips it. Non-array values are skipped and reported as `None`.
    pub fn skip_array_nonempty(&mut self) -> R<Option<bool>> {
        if self.peek()? != b'[' {
            self.skip_value()?;
            return Ok(None);
        }
        let open = self.i;
        self.i += 1;
        let nonempty = self.peek()? != b']';
        self.i = open;
        self.skip_container()?;
        Ok(Some(nonempty))
    }
}

/// Appends the decoded contents of a raw JSON string (without quotes).
/// Invalid UTF-8 and lone surrogates become U+FFFD.
pub(crate) fn decode_str(raw: &[u8], out: &mut String) {
    let mut rest = raw;
    loop {
        let Some(p) = memchr(b'\\', rest) else {
            push_utf8(out, rest);
            return;
        };
        push_utf8(out, &rest[..p]);
        let Some(&e) = rest.get(p + 1) else {
            return;
        };
        let mut next = p + 2;
        match e {
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'r' => out.push('\r'),
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'u' => {
                let hi = hex4(rest.get(p + 2..p + 6));
                next = p + 6;
                match hi {
                    Some(h @ 0xD800..=0xDBFF) => {
                        let lo = if rest.get(p + 6) == Some(&b'\\') && rest.get(p + 7) == Some(&b'u') {
                            hex4(rest.get(p + 8..p + 12))
                        } else {
                            None
                        };
                        match lo {
                            Some(l @ 0xDC00..=0xDFFF) => {
                                let c = 0x10000 + ((h - 0xD800) << 10) + (l - 0xDC00);
                                out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
                                next = p + 12;
                            }
                            _ => out.push('\u{FFFD}'),
                        }
                    }
                    Some(h) => out.push(char::from_u32(h).unwrap_or('\u{FFFD}')),
                    None => {
                        out.push('\u{FFFD}');
                        next = (p + 2).min(rest.len());
                    }
                }
            }
            other => out.push(other as char),
        }
        rest = &rest[next.min(rest.len())..];
    }
}

#[inline]
fn hex4(b: Option<&[u8]>) -> Option<u32> {
    let b = b?;
    let mut v = 0u32;
    for &c in b {
        v = v * 16 + (c as char).to_digit(16)?;
    }
    Some(v)
}

#[inline]
fn push_utf8(out: &mut String, b: &[u8]) {
    match std::str::from_utf8(b) {
        Ok(s) => out.push_str(s),
        Err(_) => out.push_str(&String::from_utf8_lossy(b)),
    }
}

/// Decodes an nbformat "multiline string" (a string, or an array of strings
/// that are concatenated) from its raw JSON bytes. Anything else yields "".
pub(crate) fn decode_multiline(raw: &[u8], out: &mut String) {
    let mut c = Cursor { b: raw, i: 0 };
    match c.peek() {
        Ok(b'"') => {
            if let Ok(s) = c.string_raw() {
                decode_str(s, out);
            }
        }
        Ok(b'[') => {
            let _ = c.array(|c, _| {
                if let Some(s) = c.opt_string_raw()? {
                    decode_str(s, out);
                }
                Ok(())
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes() {
        let mut s = String::new();
        decode_str(br#"a\"b\\c\nd\u00e9\ud83d\ude00\/x\ud800y"#, &mut s);
        assert_eq!(s, "a\"b\\c\nd\u{e9}\u{1F600}/x\u{FFFD}y");
    }

    #[test]
    fn skips_tricky_strings() {
        let src = br#"{"a": "x\\", "b": ["\"]", {"c": "}\\\"{"}], "d": 1}"#;
        let mut c = Cursor::new(src);
        let mut keys = vec![];
        c.object(|c, k| {
            keys.push(String::from_utf8(k.into_owned()).unwrap());
            c.skip_value()
        })
        .unwrap();
        assert_eq!(keys, ["a", "b", "d"]);
        assert_eq!(c.i, src.len());
    }

    #[test]
    fn escaped_keys_are_decoded() {
        let src = br#"{"cell_type": "code"}"#;
        let mut c = Cursor::new(src);
        let mut got = None;
        c.object(|c, k| {
            got = Some(k.into_owned());
            c.skip_value()
        })
        .unwrap();
        assert_eq!(got.as_deref(), Some(&b"cell_type"[..]));
    }

    #[test]
    fn multiline_forms() {
        let mut s = String::new();
        decode_multiline(br#"["a\n", "b"]"#, &mut s);
        assert_eq!(s, "a\nb");
        s.clear();
        decode_multiline(br#""a\nb""#, &mut s);
        assert_eq!(s, "a\nb");
    }

    #[test]
    fn truncated_input_errors() {
        let mut c = Cursor::new(br#"{"cells": [{"source": "abc"#);
        assert!(c.skip_value().is_err());
    }
}
