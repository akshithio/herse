//! What a cell's text *is*: number, missing value, boolean, truncation marker.

use crate::CellKind;

/// Classify rendered cell text. `italic` lets Julia's `missing` (which
/// DataFrames.jl renders in italics) count as a missing value without
/// muting every string that happens to read "missing".
pub(crate) fn classify(text: &str, italic: bool) -> CellKind {
    let t = text.trim_ascii();
    if t.is_empty() {
        return CellKind::Empty;
    }
    // Most cells are numbers: test that first.
    if is_number(t) {
        return CellKind::Number;
    }
    match t {
        "..." | "…" | "⋮" | "⋯" | "⋱" => CellKind::Ellipsis,
        "NaN" | "nan" | "-nan" | "NaT" | "nat" | "None" | "<NA>" | "NA" | "null" | "NULL" | "<null>"
        | "nil" | "N/A" | "n/a" | "-" | "–" | "—" => CellKind::Null,
        "missing" | "nothing" if italic => CellKind::Null,
        "True" | "False" | "true" | "false" | "TRUE" | "FALSE" => CellKind::Bool,
        _ => CellKind::Text,
    }
}

/// Integers, decimals, scientific notation, digit grouping (`1,234,567`,
/// `1_000`), percentages, a leading currency symbol, `inf` / `-inf`, and the
/// Unicode minus sign. Not: dates, times, version strings, ranges.
pub(crate) fn is_number(t: &str) -> bool {
    let mut b = t.as_bytes();
    // Sign and currency in either order: "-$5", "$-5", "−3" (U+2212).
    if !b.first().is_some_and(u8::is_ascii_digit) {
        for _ in 0..2 {
            b = match b {
                [b'-' | b'+', r @ ..] | [0xE2, 0x88, 0x92, r @ ..] => r,
                _ => b,
            };
            b = match b {
                // $, £, ¥, €, ₹
                [b'$', r @ ..] | [0xC2, 0xA3 | 0xA5, r @ ..] | [0xE2, 0x82, 0xAC | 0xB9, r @ ..] => r,
                _ => b,
            };
        }
        if b.eq_ignore_ascii_case(b"inf") || b.eq_ignore_ascii_case(b"infinity") {
            return true;
        }
    }
    let b = b.strip_suffix(b"%").unwrap_or(b);
    let mut i = 0;
    let mut int_digits = 0;
    while i < b.len() {
        match b[i] {
            b'0'..=b'9' => int_digits += 1,
            // Group separators only between digits, never doubled or leading.
            b',' | b'_' if int_digits > 0 && b.get(i + 1).is_some_and(u8::is_ascii_digit) => {}
            _ => break,
        }
        i += 1;
    }
    let mut frac_digits = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            frac_digits += 1;
            i += 1;
        }
    }
    if int_digits + frac_digits == 0 {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let exp_start = j;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j == exp_start {
            return false;
        }
        i = j;
    }
    i == b.len()
}

/// Terminal-style display width: East Asian wide / fullwidth characters and
/// emoji count 2, combining marks and zero-width characters 0. For a
/// multi-line cell, the widest line.
pub(crate) fn display_width(s: &str) -> usize {
    if s.is_ascii() {
        return s.split('\n').map(str::len).max().unwrap_or(0);
    }
    let mut best = 0;
    let mut cur = 0;
    for c in s.chars() {
        if c == '\n' {
            best = best.max(cur);
            cur = 0;
            continue;
        }
        cur += char_width(c);
    }
    best.max(cur)
}

fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x300 {
        return 1;
    }
    match u {
        0x300..=0x36F | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0xFE00..=0xFE0F
        | 0xFEFF | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xE0100..=0xE01EF => 0,
        // Hebrew / Arabic combining marks.
        0x591..=0x5BD | 0x610..=0x61A | 0x64B..=0x65F => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x1F680..=0x1F6FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        for s in [
            "0", "-1", "+2", "3.25", ".5", "5.", "1e-5", "1.000000e+300", "-0.000000e+00", "1,234",
            "1,234,567.89", "1_000", "12%", "-3.5%", "$5", "-$5.00", "€1,200", "−7", "inf", "-inf",
            "Infinity", "4955.50",
        ] {
            assert!(is_number(s), "{s}");
            assert_eq!(classify(s, false), CellKind::Number, "{s}");
        }
        for s in [
            "", "-", ".", "1e", "e5", "1,", ",1", "1,,2", "2024-01-01", "1.2.3", "0x10", "12 ms",
            "(1, 2]", "1/2", "$", "%", "1e5e", "infinite", "1 days 00:00:00",
        ] {
            assert!(!is_number(s), "{s}");
        }
    }

    #[test]
    fn kinds() {
        assert_eq!(classify("  NaN ", false), CellKind::Null);
        assert_eq!(classify("<NA>", false), CellKind::Null);
        assert_eq!(classify("NaT", false), CellKind::Null);
        assert_eq!(classify("null", false), CellKind::Null);
        assert_eq!(classify("missing", false), CellKind::Text);
        assert_eq!(classify("missing", true), CellKind::Null);
        assert_eq!(classify("...", false), CellKind::Ellipsis);
        assert_eq!(classify("…", false), CellKind::Ellipsis);
        assert_eq!(classify("⋮", false), CellKind::Ellipsis);
        assert_eq!(classify("True", false), CellKind::Bool);
        assert_eq!(classify("-", false), CellKind::Null); // Styler na_rep="-"
        assert_eq!(classify("alpha", false), CellKind::Text);
        assert_eq!(classify("", false), CellKind::Empty);
    }

    #[test]
    fn widths() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("a\nlonger"), 6);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("🚀 emoji"), 8);
    }
}
