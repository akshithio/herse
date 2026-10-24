//! Text edits the cell editor doesn't do by itself.

use std::ops::Range;

/// The line comment of a kernel language (by its grammar name).
pub fn comment_prefix(grammar: &str) -> &'static str {
    match grammar {
        "rust" | "javascript" | "typescript" | "tsx" | "c" | "cpp" | "java" | "scala" | "go" | "kotlin" | "swift"
        | "csharp" => "//",
        "sql" | "lua" | "haskell" => "--",
        "matlab" | "octave" => "%",
        _ => "#",
    }
}

/// Toggle line comments over the lines `range` touches: uncomment when every
/// non-blank line is commented, else comment them all at the shallowest
/// indent (as VS Code does). A selection ending at the start of a line leaves
/// that line alone. Returns the byte range of `source` to replace and its
/// replacement.
pub fn toggle_comment(source: &str, range: Range<usize>, prefix: &str) -> (Range<usize>, String) {
    let Range { start, end } = line_span(source, range);
    let lines: Vec<&str> = source[start..end].split('\n').collect();
    let blank = |l: &str| l.trim().is_empty();
    // Indentation is spaces and tabs only, so it is always whole ASCII bytes
    // (other Unicode spaces count as text, never cut through).
    let body = |l: &str| l.trim_start_matches([' ', '\t']).len();
    let indent = |l: &str| l.len() - body(l);
    let uncomment = lines.iter().any(|l| !blank(l))
        && lines.iter().filter(|l| !blank(l)).all(|l| l[indent(l)..].starts_with(prefix));
    let out: Vec<String> = if uncomment {
        lines
            .iter()
            .map(|l| {
                if blank(l) {
                    return l.to_string();
                }
                let at = indent(l);
                let rest = &l[at + prefix.len()..];
                format!("{}{}", &l[..at], rest.strip_prefix(' ').unwrap_or(rest))
            })
            .collect()
    } else {
        let col = lines.iter().filter(|l| !blank(l)).map(|l| indent(l)).min().unwrap_or(0);
        lines
            .iter()
            .map(|l| if blank(l) { l.to_string() } else { format!("{}{prefix} {}", &l[..col], &l[col..]) })
            .collect()
    };
    (start..end, out.join("\n"))
}

/// The whole lines `range` touches, without the last one's newline. A
/// selection ending at the start of a line leaves that line out.
fn line_span(source: &str, range: Range<usize>) -> Range<usize> {
    let range = range.start.min(source.len())..range.end.min(source.len());
    let start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let mut last = range.end;
    if range.end > range.start && source[..range.end].ends_with('\n') {
        last -= 1;
    }
    let from = last.max(start);
    let end = source[from..].find('\n').map_or(source.len(), |i| from + i);
    start..end
}

/// An edit: replace `range` of the source with `text`, then select `select`.
#[derive(Debug, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
    pub select: Range<usize>,
}

/// ⌥↑ / ⌥↓: swap the lines `range` touches with the line above or below,
/// the selection moving with them. `None` at the top or bottom.
pub fn move_lines(source: &str, range: Range<usize>, up: bool) -> Option<Edit> {
    let Range { start, end } = line_span(source, range.clone());
    let block = &source[start..end];
    if up {
        if start == 0 {
            return None;
        }
        let above = source[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        let shift = start - above;
        Some(Edit {
            range: above..end,
            text: format!("{block}\n{}", &source[above..start - 1]),
            select: range.start - shift..range.end - shift,
        })
    } else {
        if end == source.len() {
            return None;
        }
        let below = source[end + 1..].find('\n').map_or(source.len(), |i| end + 1 + i);
        let next = &source[end + 1..below];
        let shift = next.len() + 1;
        Some(Edit {
            range: start..below,
            text: format!("{next}\n{block}"),
            select: range.start + shift..range.end + shift,
        })
    }
}

/// ⇧⌘K: delete the lines `range` touches, newline and all; the cursor lands
/// at the start of the line that takes their place.
pub fn delete_lines(source: &str, range: Range<usize>) -> Edit {
    let Range { start, end } = line_span(source, range);
    let (range, at) = if end < source.len() {
        (start..end + 1, start)
    } else if start > 0 {
        let previous = source[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        (start - 1..end, previous)
    } else {
        (start..end, 0)
    };
    Edit { range, text: String::new(), select: at..at }
}

/// ⌥⇧↑ / ⌥⇧↓: copy the lines `range` touches above or below them. Down
/// moves the selection onto the copy; up leaves it on the upper of the two.
pub fn copy_lines(source: &str, range: Range<usize>, up: bool) -> Edit {
    let Range { start, end } = line_span(source, range.clone());
    let block = &source[start..end];
    let shift = if up { 0 } else { block.len() + 1 };
    Edit {
        range: end..end,
        text: format!("\n{block}"),
        select: range.start + shift..range.end + shift,
    }
}

/// ⌘L: select the lines `range` touches, newline included; again, and the
/// next line joins.
pub fn select_lines(source: &str, range: Range<usize>) -> Range<usize> {
    let span = line_span(source, range.clone());
    let with_newline = |end: usize| (end + 1).min(source.len());
    let whole = span.start..with_newline(span.end);
    if range == whole && whole.end < source.len() {
        // Already whole lines: take the next one too.
        let next = source[whole.end..].find('\n').map_or(source.len(), |i| whole.end + i);
        return span.start..with_newline(next);
    }
    whole
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(source: &str, range: Range<usize>) -> String {
        let (r, new) = toggle_comment(source, range, "#");
        format!("{}{}{}", &source[..r.start], new, &source[r.end..])
    }

    #[test]
    fn comments_the_cursor_line() {
        assert_eq!(apply("a = 1\nb = 2", 2..2), "# a = 1\nb = 2");
        assert_eq!(apply("a = 1\nb = 2", 8..8), "a = 1\n# b = 2");
    }

    #[test]
    fn comments_a_block_at_its_shallowest_indent() {
        let src = "if x:\n    y()\n\n    z()";
        assert_eq!(apply(src, 0..src.len()), "# if x:\n#     y()\n\n#     z()");
        let body = "if x:\n    y()\n    z()";
        assert_eq!(apply(body, 6..body.len()), "if x:\n    # y()\n    # z()");
    }

    #[test]
    fn uncomments_when_every_line_is_commented() {
        assert_eq!(apply("# a\n#b\n  # c", 0..12), "a\nb\n  c");
        // One uncommented line comments them all instead.
        assert_eq!(apply("# a\nb", 0..5), "# # a\n# b");
    }

    #[test]
    fn unicode_spaces_are_text_not_indent() {
        // A no-break space (two bytes) must never be cut in half.
        assert_eq!(apply("\u{a0}x\n y", 0..7), "# \u{a0}x\n#  y");
        assert_eq!(apply("\u{a0}# x", 0..5), "# \u{a0}# x");
    }

    #[test]
    fn a_selection_ending_at_a_line_start_skips_that_line() {
        assert_eq!(apply("a\nb\nc", 0..4), "# a\n# b\nc");
    }

    fn run(source: &str, edit: Edit) -> (String, Range<usize>) {
        (format!("{}{}{}", &source[..edit.range.start], edit.text, &source[edit.range.end..]), edit.select)
    }

    #[test]
    fn moves_lines_up_and_down() {
        let src = "a\nbb\nc";
        assert_eq!(run(src, move_lines(src, 3..3, true).unwrap()), ("bb\na\nc".into(), 1..1));
        assert_eq!(run(src, move_lines(src, 3..3, false).unwrap()), ("a\nc\nbb".into(), 5..5));
        assert_eq!(move_lines(src, 0..0, true), None);
        assert_eq!(move_lines(src, 6..6, false), None);
        // A two-line selection moves as a block.
        assert_eq!(run(src, move_lines(src, 2..6, true).unwrap()).0, "bb\nc\na");
    }

    #[test]
    fn deletes_lines() {
        let src = "a\nbb\nc";
        assert_eq!(run(src, delete_lines(src, 3..3)), ("a\nc".into(), 2..2));
        assert_eq!(run(src, delete_lines(src, 6..6)), ("a\nbb".into(), 2..2));
        assert_eq!(run("x", delete_lines("x", 0..0)), ("".into(), 0..0));
    }

    #[test]
    fn copies_lines() {
        let src = "a\nbb\nc";
        assert_eq!(run(src, copy_lines(src, 3..3, false)), ("a\nbb\nbb\nc".into(), 6..6));
        assert_eq!(run(src, copy_lines(src, 3..3, true)), ("a\nbb\nbb\nc".into(), 3..3));
        // A block copies as a block, at the end of the text too.
        assert_eq!(run(src, copy_lines(src, 2..6, false)).0, "a\nbb\nc\nbb\nc");
    }

    #[test]
    fn selects_lines() {
        let src = "a\nbb\nc";
        assert_eq!(select_lines(src, 3..3), 2..5);
        assert_eq!(select_lines(src, 2..5), 2..6);
        assert_eq!(select_lines(src, 2..6), 2..6);
        assert_eq!(select_lines("x", 0..0), 0..1);
    }

    #[test]
    fn languages() {
        assert_eq!(comment_prefix("python"), "#");
        assert_eq!(comment_prefix("rust"), "//");
        assert_eq!(comment_prefix("sql"), "--");
    }
}
