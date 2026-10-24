//! A small Python highlighter for static pages: comments, strings (prefixed,
//! triple-quoted), numbers, keywords, builtins, decorators and the names a
//! `def` or `class` introduces. A lexer, not a parser: good enough to read
//! by, and it never fails. Other languages are shown plain.

use crate::escape;

const KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "match", "case", "nonlocal", "not", "or",
    "pass", "raise", "return", "try", "while", "with", "yield",
];

const CONSTANTS: &[&str] = &["True", "False", "None", "self", "cls"];

const BUILTINS: &[&str] = &[
    "abs", "all", "any", "bool", "dict", "dir", "enumerate", "filter", "float", "format", "getattr", "hasattr", "int",
    "isinstance", "iter", "len", "list", "map", "max", "min", "next", "open", "print", "range", "repr", "reversed",
    "round", "set", "setattr", "sorted", "str", "sum", "super", "tuple", "type", "zip",
];

/// `source` as HTML with `<span class="…">` around the tokens worth a
/// color.
pub fn python(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + source.len() / 2);
    let b = source.as_bytes();
    let mut i = 0;
    // The previous word, to color the name after `def` / `class`.
    let mut prev_word = "";
    while i < b.len() {
        let c = b[i];
        // Comment, to the end of the line.
        if c == b'#' {
            let end = source[i..].find('\n').map_or(b.len(), |e| i + e);
            span(&mut out, "c", &source[i..end]);
            i = end;
            continue;
        }
        // Strings, with any prefix (r, b, f, rb, …) already in `word` below.
        if c == b'"' || c == b'\'' {
            let end = string_end(b, i);
            span(&mut out, "s", &source[i..end]);
            i = end;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let mut end = i + 1;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'.') {
                end += 1;
            }
            span(&mut out, "n", &source[i..end]);
            i = end;
            continue;
        }
        if c == b'@' && line_start(b, i) {
            let mut end = i + 1;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'.') {
                end += 1;
            }
            span(&mut out, "f", &source[i..end]);
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let mut end = i;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] >= 0x80) {
                end += 1;
            }
            // Back off to a char boundary (non-ASCII identifiers).
            while !source.is_char_boundary(end) {
                end += 1;
            }
            let word = &source[i..end];
            // A string prefix: the quote follows directly.
            if end < b.len()
                && (b[end] == b'"' || b[end] == b'\'')
                && word.len() <= 2
                && word.chars().all(|c| "rRbBfFuU".contains(c))
            {
                let close = string_end(b, end);
                span(&mut out, "s", &source[i..close]);
                i = close;
                prev_word = "";
                continue;
            }
            let class = if KEYWORDS.contains(&word) {
                Some("k")
            } else if CONSTANTS.contains(&word) {
                Some("o")
            } else if prev_word == "def" || prev_word == "class" {
                Some("f")
            } else if BUILTINS.contains(&word) && b.get(end) == Some(&b'(') {
                Some("b")
            } else if b.get(end) == Some(&b'(') {
                Some("f")
            } else {
                None
            };
            match class {
                Some(class) => span(&mut out, class, word),
                None => out.push_str(&escape(word)),
            }
            prev_word = word;
            i = end;
            continue;
        }
        // Anything else, one char at a time.
        let ch_len = source[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&escape(&source[i..i + ch_len]));
        if !c.is_ascii_whitespace() {
            prev_word = "";
        }
        i += ch_len;
    }
    out
}

fn span(out: &mut String, class: &str, text: &str) {
    out.push_str("<span class=\"");
    out.push_str(class);
    out.push_str("\">");
    out.push_str(&escape(text));
    out.push_str("</span>");
}

fn line_start(b: &[u8], i: usize) -> bool {
    b[..i].iter().rev().take_while(|&&c| c != b'\n').all(|c| *c == b' ' || *c == b'\t')
}

/// The end (exclusive) of the string literal starting at the quote at `i`.
/// An unterminated single-quoted string ends at the line's end.
fn string_end(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let triple = b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q);
    let mut j = if triple { i + 3 } else { i + 1 };
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'\n' if !triple => return j,
            c if c == q => {
                if !triple {
                    return j + 1;
                }
                if b.get(j + 1) == Some(&q) && b.get(j + 2) == Some(&q) {
                    return j + 3;
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_get_their_classes() {
        let html = python("def f(x):\n    return x + 1  # one\ns = f'a{x}' \"\"\"doc\nmore\"\"\" True\n@cache\nprint(len(s))");
        assert!(html.contains("<span class=\"k\">def</span> <span class=\"f\">f</span>"), "{html}");
        assert!(html.contains("<span class=\"n\">1</span>"), "{html}");
        assert!(html.contains("<span class=\"c\"># one</span>"), "{html}");
        assert!(html.contains("<span class=\"s\">f&#39;a{x}&#39;</span>"), "{html}");
        assert!(html.contains("<span class=\"s\">&quot;&quot;&quot;doc\nmore&quot;&quot;&quot;</span>"), "{html}");
        assert!(html.contains("<span class=\"o\">True</span>"), "{html}");
        assert!(html.contains("<span class=\"f\">@cache</span>"), "{html}");
        assert!(html.contains("<span class=\"b\">print</span>(<span class=\"b\">len</span>(s))"), "{html}");
    }

    #[test]
    fn odd_input_never_breaks() {
        for s in ["'unterminated", "\"\"\"open", "x = 'a\\'b'", "é = 1", "@", "1e-3j", "\\"] {
            let _ = python(s);
        }
        assert_eq!(python("a < b"), "a &lt; b");
    }
}
