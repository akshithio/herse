//! JupyterLab's math extraction for Markdown cells (`removeMath` /
//! `replaceMath` in `@jupyterlab/rendermime/src/latex.ts`), ported
//! literally so the same text is math in Herse as in JupyterLab.
//!
//! JupyterLab pulls math out *before* Markdown parsing (so `_`, `*` and `\\`
//! inside formulas are not mangled), renders the Markdown with `@@N@@`
//! placeholders, then puts the math back for MathJax. Recognized:
//! `$…$`, `$$…$$`, `\\(…\\)`, `\\[…\\]` (two backslashes in the Markdown
//! source, as Markdown would eat one) and `\begin{env}…\end{env}` with no
//! delimiters. Braces must balance, a blank line ends a formula, `\$` is a
//! literal dollar, and dollars inside code spans and fenced code blocks are
//! ignored.

/// Split Markdown `text` into text with `@@N@@` placeholders and the math
/// pieces (delimiters included; `\\(`/`\\[` already reduced to `\(`/`\[` as
/// `replaceMath` does). Feed each piece to [`super::render_svg`], which
/// strips the delimiters and picks inline/display from them.
pub fn remove_math(text: &str) -> (String, Vec<String>) {
    let has_code = text.contains('`') || text.contains("~~~");
    let prepared: String;
    let text = if has_code {
        let t = text.replace('~', "~T");
        let t = mask_fenced(&t);
        prepared = mask_inline_code(&t);
        &prepared[..]
    } else {
        text
    };
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut blocks = split(&normalized);
    let mut math: Vec<String> = Vec::new();
    let mut start: Option<usize> = None;
    let mut end: String = String::new();
    let mut last: Option<usize> = None;
    let mut braces = 0usize;

    let mut i = 1;
    while i < blocks.len() {
        let block = blocks[i].clone();
        if block.starts_with('@') {
            blocks[i] = format!("@@{}@@", math.len());
            math.push(block);
        } else if let Some(s) = start {
            if block == end {
                if braces > 0 {
                    last = Some(i);
                } else {
                    process(s, i, &mut math, &mut blocks, has_code);
                    start = None;
                    end.clear();
                    last = None;
                }
            } else if is_paragraph_break(&block) {
                if let Some(l) = last {
                    i = l;
                    process(s, i, &mut math, &mut blocks, has_code);
                }
                start = None;
                end.clear();
                last = None;
                braces = 0;
            } else if block == "{" {
                braces += 1;
            } else if block == "}" && braces > 0 {
                braces -= 1;
            }
        } else if block == "$" || block == "$$" {
            start = Some(i);
            end = block.clone();
            braces = 0;
        } else if block == "\\\\(" || block == "\\\\[" {
            start = Some(i);
            end = if block.ends_with('(') {
                "\\\\)".into()
            } else {
                "\\\\]".into()
            };
            braces = 0;
        } else if block.len() > 6 && block[1..].starts_with("begin") {
            start = Some(i);
            end = format!("\\end{}", &block[6..]);
            braces = 0;
        }
        i += 2;
    }
    if let (Some(s), Some(l)) = (start, last) {
        process(s, l, &mut math, &mut blocks, has_code);
    }
    let joined: String = blocks.concat();
    let out = if has_code { de_tilde(&joined) } else { joined };
    for m in &mut math {
        *m = reduce_delimiters(m);
    }
    (out, math)
}

/// Put rendered pieces back: every `@@N@@` becomes `f(N)`.
pub fn replace_math(text: &str, mut f: impl FnMut(usize) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find("@@") {
        let after = &rest[p + 2..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with("@@") {
            out.push_str(&rest[..p]);
            match after[..digits].parse::<usize>() {
                Ok(n) => out.push_str(&f(n)),
                Err(_) => out.push_str(&rest[p..p + 4 + digits]),
            }
            rest = &after[digits + 2..];
        } else {
            out.push_str(&rest[..p + 2]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn reduce_delimiters(m: &str) -> String {
    if m.len() >= 6 && m.starts_with("\\\\(") && m.ends_with("\\\\)") {
        format!("\\({}\\)", &m[3..m.len() - 3])
    } else if m.len() >= 6 && m.starts_with("\\\\[") && m.ends_with("\\\\]") {
        format!("\\[{}\\]", &m[3..m.len() - 3])
    } else {
        m.to_string()
    }
}

/// JS `/\n.*\n/`: two line feeds in one whitespace block.
fn is_paragraph_break(block: &str) -> bool {
    block.bytes().filter(|&b| b == b'\n').count() >= 2
}

fn process(i: usize, j: usize, math: &mut Vec<String>, blocks: &mut [String], has_code: bool) {
    let block: String = blocks[i..=j].concat();
    for b in &mut blocks[i + 1..=j] {
        b.clear();
    }
    blocks[i] = format!("@@{}@@", math.len());
    math.push(if has_code { de_tilde(&block) } else { block });
}

fn de_tilde(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '~' {
            match it.peek() {
                Some('T') => {
                    it.next();
                    out.push('~');
                    continue;
                }
                Some('D') => {
                    it.next();
                    out.push('$');
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// `text.split(MATHSPLIT)`: alternating text / delimiter blocks.
fn split(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut blocks = Vec::new();
    let mut plain = 0;
    let mut i = 0;
    while i < b.len() {
        if let Some(len) = delim_at(text, i) {
            blocks.push(text[plain..i].to_string());
            blocks.push(text[i..i + len].to_string());
            i += len;
            plain = i;
        } else {
            i += utf8_len(b[i]);
        }
    }
    blocks.push(text[plain..].to_string());
    blocks
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Length of a MATHSPLIT match starting at `i`, trying the alternatives in
/// order: `\$\$?`, `\\(?:begin|end)\{[a-z]*\*?\}` (case-insensitive),
/// `\\[{}$]`, `[{}]`, `(?:\n\s*)+`, `@@\d+@@`, `\\\\[()[\]]`.
fn delim_at(text: &str, i: usize) -> Option<usize> {
    let b = text.as_bytes();
    let c = b[i];
    let at = |k: usize| b.get(i + k).copied();
    match c {
        b'$' => Some(if at(1) == Some(b'$') { 2 } else { 1 }),
        b'\\' => {
            let rest = &b[i + 1..];
            for kw in [&b"begin"[..], &b"end"[..]] {
                if rest.len() > kw.len()
                    && rest[..kw.len()].eq_ignore_ascii_case(kw)
                    && rest[kw.len()] == b'{'
                {
                    let mut k = kw.len() + 1;
                    while k < rest.len() && rest[k].is_ascii_alphabetic() {
                        k += 1;
                    }
                    if k < rest.len() && rest[k] == b'*' {
                        k += 1;
                    }
                    if k < rest.len() && rest[k] == b'}' {
                        return Some(k + 2);
                    }
                }
            }
            match at(1) {
                Some(b'{' | b'}' | b'$') => Some(2),
                Some(b'\\') if matches!(at(2), Some(b'(' | b')' | b'[' | b']')) => Some(3),
                _ => None,
            }
        }
        b'{' | b'}' => Some(1),
        b'\n' => {
            // `\n\s*` greedy (JS `\s` includes line breaks).
            let mut k = i + 1;
            while k < b.len() {
                let ch = text[k..].chars().next().unwrap();
                if js_space(ch) {
                    k += ch.len_utf8();
                } else {
                    break;
                }
            }
            Some(k - i)
        }
        b'@' if at(1) == Some(b'@') => {
            let mut k = 2;
            while matches!(at(k), Some(d) if d.is_ascii_digit()) {
                k += 1;
            }
            if k > 2 && at(k) == Some(b'@') && at(k + 1) == Some(b'@') {
                Some(k + 2)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn js_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `/^(?<fence>`{3,}|(~T){3,})[^`\n]*\n([\s\S]*?)^\k<fence>`*$/gm` with every
/// `$` in a match replaced by `~D`.
fn mask_fenced(t: &str) -> String {
    let b = t.as_bytes();
    let mut out = String::with_capacity(t.len());
    let mut copied = 0;
    let mut line = 0;
    while line < b.len() {
        let fence_len = fence_at(b, line);
        let line_end = memchr::memchr(b'\n', &b[line..]).map_or(b.len(), |p| line + p);
        if fence_len > 0 && !b[line + fence_len..line_end].contains(&b'`') && line_end < b.len() {
            let fence = &b[line..line + fence_len];
            // Lazily look for a closing line: fence + backticks* + EOL.
            let mut l = line_end + 1;
            let mut found = None;
            loop {
                let le = memchr::memchr(b'\n', &b[l.min(b.len())..]).map_or(b.len(), |p| l + p);
                if b[l.min(b.len())..].starts_with(fence)
                    && b[l + fence.len()..le].iter().all(|&c| c == b'`')
                {
                    found = Some(le);
                    break;
                }
                if le >= b.len() {
                    break;
                }
                l = le + 1;
            }
            if let Some(end) = found {
                out.push_str(&t[copied..line]);
                out.push_str(&t[line..end].replace('$', "~D"));
                copied = end;
                line = if end < b.len() { end + 1 } else { end };
                // The `\n` after the match is copied with the next chunk.
                continue;
            }
        }
        line = line_end + 1;
    }
    out.push_str(&t[copied.min(t.len())..]);
    out
}

/// Length of a ```` ``` ```` (3+ backticks) or `~T~T~T` (3+) fence at a line start.
fn fence_at(b: &[u8], i: usize) -> usize {
    let n = b[i..].iter().take_while(|&&c| c == b'`').count();
    if n >= 3 {
        return n;
    }
    let mut k = i;
    let mut pairs = 0;
    while b.get(k) == Some(&b'~') && b.get(k + 1) == Some(&b'T') {
        k += 2;
        pairs += 1;
    }
    if pairs >= 3 { k - i } else { 0 }
}

/// `/(^|[^\\])(`+)([^\n]*?[^`\n])\2(?!`)/gm` with `$` -> `~D` in each match,
/// including JavaScript's backtracking on the backtick run length.
fn mask_inline_code(t: &str) -> String {
    let b = t.as_bytes();
    let mut out = String::with_capacity(t.len());
    let mut copied = 0;
    let mut k = 0;
    while k < b.len() {
        match inline_match(t, k) {
            Some(end) => {
                out.push_str(&t[copied..k]);
                out.push_str(&t[k..end].replace('$', "~D"));
                copied = end;
                k = end;
            }
            None => k += utf8_len(b[k]),
        }
    }
    out.push_str(&t[copied..]);
    out
}

fn inline_match(t: &str, k: usize) -> Option<usize> {
    let b = t.as_bytes();
    let line_start = k == 0 || b[k - 1] == b'\n' || b[k - 1] == b'\r';
    // Alternative 1: `^` (zero width) — only at a line start.
    if line_start && let Some(e) = backtick_span(b, k) {
        return Some(e);
    }
    // Alternative 2: one char that is not a backslash.
    if b[k] != b'\\' {
        let next = k + utf8_len(b[k]);
        if next <= b.len()
            && let Some(e) = backtick_span(b, next)
        {
            return Some(e);
        }
    }
    None
}

/// `(`+)([^\n]*?[^`\n])\2(?!`)` starting at `s`; returns the match end.
fn backtick_span(b: &[u8], s: usize) -> Option<usize> {
    let run = b[s.min(b.len())..]
        .iter()
        .take_while(|&&c| c == b'`')
        .count();
    if run == 0 {
        return None;
    }
    let line_end = memchr::memchr(b'\n', &b[s..]).map_or(b.len(), |p| s + p);
    for n in (1..=run).rev() {
        let cs = s + n;
        // Lazy content: shortest first.
        let mut e = cs + 1;
        while e <= line_end {
            let last = b[e - 1];
            if last != b'`'
                && last != b'\n'
                && b.len() >= e + n
                && b[e..e + n].iter().all(|&c| c == b'`')
                && b.get(e + n) != Some(&b'`')
            {
                return Some(e + n);
            }
            e += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_and_display() {
        let (t, m) = remove_math("Let $x_1$ and $$\\sum_i a_i$$ be.");
        assert_eq!(t, "Let @@0@@ and @@1@@ be.");
        assert_eq!(m, vec!["$x_1$", "$$\\sum_i a_i$$"]);
    }

    #[test]
    fn environments_without_dollars() {
        let src = "Text\n\\begin{align}\na &= b \\\\\nc &= d\n\\end{align}\nmore";
        let (t, m) = remove_math(src);
        assert_eq!(t, "Text\n@@0@@\nmore");
        assert_eq!(m[0], "\\begin{align}\na &= b \\\\\nc &= d\n\\end{align}");
    }

    #[test]
    fn escaped_dollars_and_paragraphs() {
        let (t, m) = remove_math("costs \\$5 and \\$6");
        assert!(m.is_empty());
        assert_eq!(t, "costs \\$5 and \\$6");
        let (_, m) = remove_math("a $b\n\nc$ d");
        assert!(m.is_empty(), "blank line ends math");
    }

    #[test]
    fn braces_must_balance() {
        let (_, m) = remove_math("$\\text{a $ b}$");
        assert_eq!(m, vec!["$\\text{a $ b}$"]);
    }

    #[test]
    fn code_spans_are_not_math() {
        let (t, m) = remove_math("`$HOME` and `$PATH` are variables, but $x$ is math");
        assert_eq!(m, vec!["$x$"]);
        assert_eq!(t, "`$HOME` and `$PATH` are variables, but @@0@@ is math");
        let (t, m) = remove_math("```bash\necho $A $B\n```\n$y$");
        assert_eq!(m, vec!["$y$"]);
        assert!(t.starts_with("```bash\necho $A $B\n```"));
        let (t, _) = remove_math("a ~ b `c`");
        assert_eq!(t, "a ~ b `c`");
    }

    #[test]
    fn double_backslash_delimiters() {
        let (t, m) = remove_math("inline \\\\(a+b\\\\) and \\\\[c\\\\]");
        assert_eq!(t, "inline @@0@@ and @@1@@");
        assert_eq!(m, vec!["\\(a+b\\)", "\\[c\\]"]);
    }

    #[test]
    fn placeholders_roundtrip() {
        let (t, m) = remove_math("x @@3@@ $y$");
        assert_eq!(m, vec!["@@3@@", "$y$"]);
        assert_eq!(replace_math(&t, |n| m[n].clone()), "x @@3@@ $y$");
    }
}
