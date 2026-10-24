//! Text measurement (Helvetica/Arial metrics) and plotly's pseudo-HTML labels.

use crate::color::Rgba;
use crate::svg::Run;

/// Helvetica advance widths (1/1000 em) for ASCII 32..=126.
const REGULAR: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0-9
    278, 278, 584, 584, 584, 556, 1015, // :;<=>?@
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, // A-Z
    278, 278, 278, 469, 556, 333, // [\]^_`
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
    278, 556, 500, 722, 500, 500, 500, // a-z
    334, 260, 334, 584, // {|}~
];

const BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0-9
    333, 333, 584, 584, 584, 611, 975, // :;<=>?@
    722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, // A-Z
    333, 278, 333, 584, 556, 333, // [\]^_`
    556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556,
    333, 611, 556, 778, 556, 556, 500, // a-z
    389, 280, 389, 584, // {|}~
];

fn char_width(c: char, bold: bool) -> f64 {
    let code = c as u32;
    let w = if (32..=126).contains(&code) {
        let t = if bold { &BOLD } else { &REGULAR };
        t[(code - 32) as usize] as f64
    } else if c == '\u{2212}' || c == '\u{2013}' {
        584.
    } else if c == '\u{2014}' {
        1000.
    } else if c == '\u{00a0}' {
        278.
    } else if is_wide(code) {
        1000.
    } else if c.is_alphabetic() {
        if c.is_uppercase() { 680. } else { 540. }
    } else {
        556.
    };
    w / 1000.
}

fn is_wide(code: u32) -> bool {
    matches!(code,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 |
        0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD)
}

/// Approximate rendered width of `s` in px.
pub fn width(s: &str, size: f64, bold: bool) -> f64 {
    s.chars().map(|c| char_width(c, bold)).sum::<f64>() * size
}

pub fn runs_width(runs: &[Run], size: f64) -> f64 {
    runs.iter()
        .map(|r| width(&r.text, if r.shift != 0 { size * 0.7 } else { size }, r.bold))
        .sum()
}

/// Truncates to `max` px with an ellipsis (Vega's `labelLimit`).
pub fn truncate(s: &str, size: f64, bold: bool, max: f64) -> String {
    if max <= 0. || width(s, size, bold) <= max {
        return s.to_string();
    }
    let ell = width("…", size, bold);
    let mut w = 0.;
    let mut out = String::new();
    for c in s.chars() {
        let cw = char_width(c, bold) * size;
        if w + cw + ell > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// Parses plotly's label markup (`<br>`, `<b>`, `<i>`, `<sup>`, `<sub>`,
/// `<span style="color:..">`, `<a>`, entities) into lines of runs.
pub fn rich(s: &str) -> Vec<Vec<Run>> {
    let mut lines: Vec<Vec<Run>> = vec![Vec::new()];
    let mut bold = 0i32;
    let mut italic = 0i32;
    let mut shift: Vec<i8> = Vec::new();
    let mut colors: Vec<Option<Rgba>> = Vec::new();
    let mut buf = String::new();
    let flush = |lines: &mut Vec<Vec<Run>>, buf: &mut String, bold: i32, italic: i32, shift: &[i8], colors: &[Option<Rgba>]| {
        if buf.is_empty() {
            return;
        }
        let text = decode_entities(buf);
        buf.clear();
        let run = Run {
            text,
            bold: bold > 0,
            italic: italic > 0,
            shift: shift.last().copied().unwrap_or(0),
            color: colors.iter().rev().find_map(|c| *c),
        };
        if let Some(line) = lines.last_mut() {
            line.push(run);
        }
    };
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('<')
            && stripped.trim_start_matches('/').starts_with(|c: char| c.is_ascii_alphabetic())
            && let Some(end) = stripped.find('>')
        {
            let tag = &stripped[..end];
            let lower = tag.trim().to_ascii_lowercase();
            let (closing, name) = match lower.strip_prefix('/') {
                Some(n) => (true, n.trim()),
                None => (false, lower.as_str()),
            };
            let name_only = name.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
            let known = matches!(name_only, "br" | "b" | "i" | "em" | "strong" | "sup" | "sub" | "span" | "a" | "s" | "u");
            if known {
                flush(&mut lines, &mut buf, bold, italic, &shift, &colors);
                match (name_only, closing) {
                    ("br", _) => lines.push(Vec::new()),
                    ("b" | "strong", false) => bold += 1,
                    ("b" | "strong", true) => bold -= 1,
                    ("i" | "em", false) => italic += 1,
                    ("i" | "em", true) => italic -= 1,
                    ("sup", false) => shift.push(1),
                    ("sub", false) => shift.push(-1),
                    ("sup" | "sub", true) => {
                        shift.pop();
                    }
                    ("span" | "a", false) => colors.push(style_color(tag)),
                    ("span" | "a", true) => {
                        colors.pop();
                    }
                    _ => {}
                }
                rest = &stripped[end + 1..];
                continue;
            }
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            buf.push(c);
        }
        rest = chars.as_str();
    }
    flush(&mut lines, &mut buf, bold, italic, &shift, &colors);
    lines
}

fn style_color(tag: &str) -> Option<Rgba> {
    let lower = tag.to_ascii_lowercase();
    let i = lower.find("color:")?;
    let v = &tag[i + 6..];
    let end = v.find([';', '"', '\'']).unwrap_or(v.len());
    Rgba::parse(v[..end].trim())
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail[..tail.len().min(12)].find(';');
        let decoded = end.and_then(|e| {
            let ent = &tail[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "times" => Some('×'),
                "deg" => Some('°'),
                "mu" => Some('μ'),
                _ => {
                    if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else if let Some(dec) = ent.strip_prefix('#') {
                        dec.parse::<u32>().ok().and_then(char::from_u32)
                    } else {
                        None
                    }
                }
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Plain text of a rich label (for measuring / legends).
pub fn plain(s: &str) -> String {
    rich(s)
        .iter()
        .map(|l| l.iter().map(|r| r.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_plausible() {
        let w = width("Hello", 12., false);
        assert!(w > 25. && w < 32., "{w}");
        assert!(width("Hello", 12., true) > w);
        assert!(width("日本", 10., false) == 20.);
    }

    #[test]
    fn rich_text() {
        let l = rich("a<b>b</b><br>x<sup>2</sup> &amp; y");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0][1].text, "b");
        assert!(l[0][1].bold);
        assert_eq!(l[1][1].shift, 1);
        assert_eq!(l[1][2].text, " & y");
        assert_eq!(plain("<b>Title</b> <i>x</i>"), "Title x");
        // Not a tag: kept verbatim.
        assert_eq!(plain("a < b > c"), "a < b > c");
        assert_eq!(plain("x<y"), "x<y");
        assert_eq!(plain("&#x41;&#66;&bogus;"), "AB&bogus;");
    }

    #[test]
    fn truncates() {
        let t = truncate("a very long label indeed", 10., false, 40.);
        assert!(t.ends_with('…'));
        assert!(width(&t, 10., false) <= 40.5);
    }
}
