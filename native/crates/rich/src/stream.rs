//! Terminal semantics for Jupyter stream text (`\r`, `\b`, `\n`), exactly as
//! JupyterLab applies them, plus truncation for huge outputs.
//!
//! # JupyterLab's model
//!
//! JupyterLab 4 (`@jupyterlab/outputarea`, `Private.processText` /
//! `addText`) keeps a stream output as `text` plus a cursor index that
//! survives between messages, and handles each character of new text:
//!
//! * printable runs **overwrite** at the cursor (`text[..i] + run +
//!   text[i + run.len()..]`) and the cursor moves past them; text after
//!   the cursor that is not overwritten stays, so `"hello\rab"` is `"abllo"`;
//! * `\r` moves the cursor to the start of the current line (`\r\n` is
//!   therefore just a newline);
//! * `\n` appends a newline at the **end** of the text and puts the cursor
//!   there (the rest of the line survives even if the cursor was mid-line);
//! * `\b` deletes the character before the cursor *and the one at the
//!   cursor* (`text[..i-1] + text[i+1..]`, a JupyterLab quirk that only shows
//!   when the cursor is mid-line), never deleting across a line break; the
//!   `\b` itself is never inserted.
//!
//! Indices are UTF-16 code units, as in JavaScript; this module counts the
//! same way, so overwriting part of an astral character (emoji) leaves a
//! lone surrogate, which is rendered as U+FFFD like a browser would.
//!
//! ANSI escapes get no special treatment: JupyterLab runs this on the raw
//! text and renders colors afterwards, so an escape sequence counts as its
//! characters. Overwriting `"\x1b[31mhello"` with `"\rab"` leaves
//! `"ab31mhello"`, the same garbage JupyterLab shows. Typical producers (tqdm,
//! pip, rich) redraw whole lines with escapes at the same offsets, which
//! works fine.
//!
//! # Incremental streaming
//!
//! [`append`] continues from an earlier buffer. Because the cursor must
//! persist between messages and only a `String` is kept, a buffer whose
//! cursor is not at the end ends with a marker: `\r` followed by the text
//! before the cursor (`"abllo\rab"` = text `"abllo"`, cursor after `ab`).
//! That form is plain JupyterLab input, so [`normalize`] and JupyterLab
//! itself read it back to the right text (saving it in a notebook is fine).
//! Show [`visible`]`(buf)`, which drops the marker without copying.

use std::borrow::Cow;

/// Apply `\r` / `\b` to a whole stream text, the way JupyterLab does when it
/// loads or receives a stream output. Borrowed when the text has neither.
pub fn normalize(text: &str) -> Cow<'_, str> {
    if memchr::memchr2(b'\r', 0x08, text.as_bytes()).is_none() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    process_lines(&mut out, text, false);
    Cow::Owned(out)
}

/// Append a stream chunk to `buf` with JupyterLab's semantics, continuing at
/// the cursor left by the previous chunk (see the module docs for how the
/// cursor is stored). A chunk may end mid-line or with a `\r`.
///
/// `buf` must be the result of earlier [`append`]/[`normalize`] calls (or
/// empty). Cost is proportional to the chunk plus the current last line.
pub fn append(buf: &mut String, chunk: &str) {
    let b = buf.as_bytes();
    // Is there a cursor marker (or stray `\r`/`\b`) on the last line?
    let marker = match memchr::memrchr3(b'\n', b'\r', 0x08, b) {
        Some(i) => b[i] != b'\n',
        None => false,
    };
    if !marker && memchr::memchr2(b'\r', 0x08, chunk.as_bytes()).is_none() {
        // Cursor at the end, nothing that moves it: plain append.
        buf.push_str(chunk);
        return;
    }
    // Re-run the last (raw) line together with the chunk.
    let line_start = memchr::memrchr(b'\n', b).map_or(0, |i| i + 1);
    let mut raw = String::with_capacity(buf.len() - line_start + chunk.len());
    raw.push_str(&buf[line_start..]);
    raw.push_str(chunk);
    buf.truncate(line_start);
    process_lines(buf, &raw, true);
}

/// The displayable part of a buffer built by [`append`]: everything except a
/// trailing cursor marker. Never copies.
pub fn visible(buf: &str) -> &str {
    let b = buf.as_bytes();
    match memchr::memrchr2(b'\n', b'\r', b) {
        Some(i) if b[i] == b'\r' => &buf[..i],
        _ => buf,
    }
}

/// Process `text` line by line, appending the result to `out`. With
/// `keep_cursor`, a final partial line whose cursor is not at its end gets
/// the `\r` + prefix marker.
fn process_lines(out: &mut String, text: &str, keep_cursor: bool) {
    let b = text.as_bytes();
    let mut line = Line::default();
    let mut start = 0;
    loop {
        let end = memchr::memchr(b'\n', &b[start..]).map(|i| start + i);
        let seg = &text[start..end.unwrap_or(b.len())];
        let last = keep_cursor && end.is_none();
        let sb = seg.as_bytes();
        if memchr::memchr(0x08, sb).is_some() {
            line.reset();
            line.feed(seg);
            line.write(out, last);
        } else if memchr::memchr(b'\r', sb).is_some() {
            overwrite_runs(out, seg, last);
        } else {
            out.push_str(seg);
        }
        match end {
            Some(e) => {
                out.push('\n');
                start = e + 1;
            }
            None => break,
        }
    }
}

/// A line with `\r` but no `\b`: every run between `\r`s is written from
/// the line start, so position `p` ends up holding the character of the
/// latest run longer than `p`. Walk the runs backwards and only copy the
/// parts that survive (tqdm lines with thousands of redraws cost one scan).
fn overwrite_runs(out: &mut String, seg: &str, keep_cursor: bool) {
    let mut runs = seg.rsplit('\r');
    let last = runs.next().unwrap_or("");
    let line_start = out.len();
    out.push_str(last);
    let cursor = utf16_len(last);
    let mut len = cursor;
    for run in runs {
        // Cheap reject before counting: a run with fewer bytes than `len`
        // units cannot be longer.
        if run.len() <= len {
            continue;
        }
        let l = utf16_len(run);
        if l > len {
            let (off, straddle) = split_at_units(run, len);
            if straddle {
                out.push('\u{fffd}');
            }
            out.push_str(&run[off..]);
            len = l;
        }
    }
    if keep_cursor && cursor < len {
        out.push('\r');
        out.push_str(last);
    }
    debug_assert!(out.len() >= line_start);
}

/// Length in UTF-16 code units.
fn utf16_len(s: &str) -> usize {
    let mut n = 0;
    for &b in s.as_bytes() {
        // Every non-continuation byte starts a char; 4-byte leaders need two units.
        n += ((b & 0xc0) != 0x80) as usize + (b >= 0xf0) as usize;
    }
    n
}

/// Byte offset of UTF-16 unit `k` in `s`; `straddle` when `k` falls inside a
/// surrogate pair (the offset is then after that character).
fn split_at_units(s: &str, k: usize) -> (usize, bool) {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units == k {
            return (i, false);
        }
        units += c.len_utf16();
        if units > k {
            return (i + c.len_utf8(), true);
        }
    }
    (s.len(), false)
}

/// One line being edited, in UTF-16 code units like JupyterLab.
#[derive(Default)]
struct Line {
    units: Vec<u16>,
    cursor: usize,
}

impl Line {
    fn reset(&mut self) {
        self.units.clear();
        self.cursor = 0;
    }

    /// Feed a segment that contains no `\n`.
    fn feed(&mut self, seg: &str) {
        for u in seg.encode_utf16() {
            match u {
                0x0d => self.cursor = 0, // \r: start of line
                0x08 => {
                    // \b: JupyterLab removes text[i-1] and text[i].
                    if self.cursor > 0 {
                        let i = self.cursor - 1;
                        let end = (i + 2).min(self.units.len());
                        self.units.drain(i..end);
                        self.cursor = i;
                    }
                }
                _ => {
                    if self.cursor < self.units.len() {
                        self.units[self.cursor] = u;
                    } else {
                        self.units.push(u);
                    }
                    self.cursor += 1;
                }
            }
        }
    }

    fn write(&self, out: &mut String, keep_cursor: bool) {
        push_utf16(out, &self.units);
        if keep_cursor && self.cursor < self.units.len() {
            out.push('\r');
            push_utf16(out, &self.units[..self.cursor]);
        }
    }
}

fn push_utf16(out: &mut String, units: &[u16]) {
    out.extend(char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or('\u{fffd}')));
}

/// Number of lines as an output area shows them: `\n`-terminated lines plus
/// a final unterminated one (so `""` is 0, `"a"` and `"a\n"` are 1).
pub fn line_count(text: &str) -> usize {
    let n = memchr::memchr_iter(b'\n', text.as_bytes()).count();
    if text.is_empty() || text.ends_with('\n') {
        n
    } else {
        n + 1
    }
}

/// Byte offset just past the `n`-th line (its `\n` included), or `text.len()`.
fn end_of_lines(text: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    match memchr::memchr_iter(b'\n', text.as_bytes()).nth(n - 1) {
        Some(i) => i + 1,
        None => text.len(),
    }
}

/// Largest char boundary `<= i`.
fn floor_boundary(text: &str, mut i: usize) -> usize {
    if i >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Keep the first `max_lines` lines of `text`, and at most `max_bytes` bytes
/// of them, for outputs too large to lay out (a 100k-line `print` loop).
///
/// Returns the text to show and, if something was cut, how many lines are
/// hidden after it (a line cut in the middle counts as hidden). The cut is at
/// a line end when one fits in `max_bytes`, otherwise inside the first line
/// at a char boundary. The shown head has no trailing `\n`, so the app can
/// put its "N more lines" row right below. Never copies; costs one `memchr`
/// pass over the text.
pub fn truncate_for_display(
    text: &str,
    max_lines: usize,
    max_bytes: usize,
) -> (Cow<'_, str>, Option<usize>) {
    let total = line_count(text);
    if total <= max_lines && text.len() <= max_bytes {
        return (Cow::Borrowed(text), None);
    }
    let cut = cut_head(text, max_lines, max_bytes);
    let head = &text[..cut];
    let shown_full = memchr::memchr_iter(b'\n', head.as_bytes()).count();
    let hidden = total - shown_full;
    let head = head.strip_suffix('\n').unwrap_or(head);
    (Cow::Borrowed(head), Some(hidden))
}

fn cut_head(text: &str, max_lines: usize, max_bytes: usize) -> usize {
    let by_lines = end_of_lines(text, max_lines);
    if by_lines <= max_bytes {
        return by_lines;
    }
    let limit = floor_boundary(text, max_bytes);
    match memchr::memrchr(b'\n', &text.as_bytes()[..limit]) {
        Some(i) => i + 1,
        None => limit,
    }
}

/// Head and tail of a long text with the middle elided; see [`head_tail`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadTail<'a> {
    /// First lines (no trailing `\n`).
    pub head: &'a str,
    /// Lines left out between `head` and `tail`.
    pub hidden_lines: usize,
    /// Last lines.
    pub tail: &'a str,
}

/// Like [`truncate_for_display`] but keeps both ends: up to `head_lines`
/// first lines and `tail_lines` last lines (training logs care about the
/// latest epoch), each limited to `max_bytes / 2`. `None` when the text fits
/// (`head_lines + tail_lines` lines and `max_bytes`).
pub fn head_tail(
    text: &str,
    head_lines: usize,
    tail_lines: usize,
    max_bytes: usize,
) -> Option<HeadTail<'_>> {
    let total = line_count(text);
    if total <= head_lines + tail_lines && text.len() <= max_bytes {
        return None;
    }
    let half = max_bytes / 2;
    let head_end = cut_head(text, head_lines, half);
    // Tail: the last `tail_lines` lines, then shrink to `half` bytes at a line
    // start (or a char boundary inside the last line).
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut tail_start = body.len();
    let mut found = 0;
    for i in memchr::memrchr_iter(b'\n', body.as_bytes()) {
        if found == tail_lines {
            break;
        }
        tail_start = i + 1;
        found += 1;
    }
    if found < tail_lines {
        tail_start = 0;
    }
    if tail_lines == 0 {
        tail_start = text.len();
    }
    if text.len() - tail_start > half {
        let min_start = text.len() - half;
        tail_start = match memchr::memchr(b'\n', &text.as_bytes()[min_start..]) {
            Some(i) if min_start + i + 1 < text.len() => min_start + i + 1,
            _ => {
                let mut s = min_start;
                while !text.is_char_boundary(s) {
                    s += 1;
                }
                s
            }
        };
    }
    let tail_start = tail_start.max(head_end);
    let head = &text[..head_end];
    let middle = &text[head_end..tail_start];
    let hidden_lines = line_count(middle);
    Some(HeadTail {
        head: head.strip_suffix('\n').unwrap_or(head),
        hidden_lines,
        tail: &text[tail_start..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal port of JupyterLab's `Private.processText` (UTF-16 indices),
    /// used as the oracle.
    fn jlab(index: usize, new_text: &str, text: &str) -> (String, usize) {
        let mut t: Vec<u16> = text.encode_utf16().collect();
        let nt: Vec<u16> = new_text.encode_utf16().collect();
        let mut idx0 = index;
        if !nt.iter().any(|&c| c == 8 || c == 13 || c == 10) {
            let mut out = t[..idx0].to_vec();
            out.extend_from_slice(&nt);
            if idx0 + nt.len() < t.len() {
                out.extend_from_slice(&t[idx0 + nt.len()..]);
            }
            return (String::from_utf16_lossy(&out), idx0 + nt.len());
        }
        let mut last_end = 0;
        loop {
            let idx1 = nt[last_end.min(nt.len())..]
                .iter()
                .position(|&c| c == 8 || c == 13 || c == 10)
                .map(|p| p + last_end);
            let prefix = &nt[last_end.min(nt.len())..idx1.unwrap_or(nt.len())];
            let mut out = t[..idx0].to_vec();
            out.extend_from_slice(prefix);
            if idx0 + prefix.len() < t.len() {
                out.extend_from_slice(&t[idx0 + prefix.len()..]);
            }
            t = out;
            idx0 += prefix.len();
            let Some(idx1) = idx1 else { break };
            last_end = idx1 + 1;
            match nt[idx1] {
                8 => {
                    if idx0 > 0 && t[idx0 - 1] != 10 {
                        let mut out = t[..idx0 - 1].to_vec();
                        if idx0 + 1 < t.len() {
                            out.extend_from_slice(&t[idx0 + 1..]);
                        }
                        t = out;
                        idx0 -= 1;
                    }
                }
                13 => {
                    while idx0 > 0 && t[idx0 - 1] != 10 {
                        idx0 -= 1;
                    }
                }
                _ => {
                    t.push(10);
                    idx0 = t.len();
                }
            }
        }
        (String::from_utf16_lossy(&t), idx0)
    }

    #[test]
    fn carriage_return_overwrites() {
        assert_eq!(normalize("hello\rab"), "abllo");
        assert_eq!(normalize("hello\rab\nnext"), "abllo\nnext");
        assert_eq!(normalize("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(
            normalize("progress 10%\rprogress 20%\rprogress 30%\n"),
            "progress 30%\n"
        );
        assert_eq!(normalize("abc\r"), "abc");
        assert_eq!(normalize("\r\rx"), "x");
    }

    #[test]
    fn backspace_quirks() {
        assert_eq!(normalize("abc\x08d"), "abd");
        assert_eq!(normalize("ab\x08\x08\x08c"), "c");
        assert_eq!(normalize("a\n\x08b"), "a\nb");
        // Mid-line backspace also eats the character at the cursor.
        assert_eq!(normalize("abcdef\rxy\x08z"), "xzef");
    }

    #[test]
    fn fast_path_borrows() {
        assert!(matches!(normalize("plain\ntext\n"), Cow::Borrowed(_)));
        let mut b = String::from("x");
        append(&mut b, "yz\n");
        assert_eq!(b, "xyz\n");
    }

    #[test]
    fn ansi_inside_overwrite_matches_jupyterlab() {
        assert_eq!(normalize("\x1b[31mhello\rab"), "ab31mhello");
    }

    #[test]
    fn tqdm_stream() {
        let mut buf = String::new();
        for i in 0..=10 {
            append(
                &mut buf,
                &format!("\r{:3}%|{:<10}| {}/10", i * 10, "#".repeat(i), i),
            );
            assert_eq!(visible(&buf).matches('%').count(), 1);
        }
        append(&mut buf, "\n");
        append(&mut buf, "done\n");
        assert_eq!(buf, "100%|##########| 10/10\ndone\n");
    }

    #[test]
    fn print_end_cr() {
        // print(i, end="\r") then a shorter print: the cursor must persist.
        let mut buf = String::new();
        append(&mut buf, "100\r");
        assert_eq!(visible(&buf), "100");
        append(&mut buf, "7");
        assert_eq!(visible(&buf), "700");
        assert_eq!(buf, "700\r7");
        append(&mut buf, "\n");
        assert_eq!(buf, "700\n");
        assert_eq!(normalize("700\r7"), "700");
    }

    #[test]
    fn append_matches_oracle_for_any_split() {
        let cases = [
            "hello\rab\ncd\x08\x08e\r\nfoo\rbarbaz\rq",
            "abc\x08\x08\x08\x08xyz\r12\x083\n\r\r\x08",
            "😀abc\rx\x08\x08y\n日本語\r語\x08",
            "\x1b[32m 50%\x1b[0m\r\x1b[32m100%\x1b[0m\n",
            "a\rb\rc\rd\re",
            "\x08\x08start\n\x08",
        ];
        for case in cases {
            let (want, _) = jlab(0, case, "");
            assert_eq!(normalize(case), want, "normalize {case:?}");
            // Every split into two chunks, and char-by-char.
            let chars: Vec<(usize, char)> = case.char_indices().collect();
            for &(k, _) in &chars {
                let mut buf = String::new();
                append(&mut buf, &case[..k]);
                append(&mut buf, &case[k..]);
                let (text, idx) = {
                    let (t1, i1) = jlab(0, &case[..k], "");
                    jlab(i1, &case[k..], &t1)
                };
                assert_eq!(visible(&buf), text, "split at {k} of {case:?}");
                // The stored cursor must be equivalent: appending more agrees.
                let mut more = buf.clone();
                append(&mut more, "ZZ");
                let (t2, _) = jlab(idx, "ZZ", &text);
                assert_eq!(visible(&more), t2, "continue after split {k} of {case:?}");
            }
            let mut buf = String::new();
            let mut t = String::new();
            let mut idx = 0;
            for (_, c) in &chars {
                let s = c.to_string();
                append(&mut buf, &s);
                let r = jlab(idx, &s, &t);
                t = r.0;
                idx = r.1;
                assert_eq!(visible(&buf), t, "char-by-char {case:?}");
            }
        }
    }

    #[test]
    fn pseudo_random_against_oracle() {
        let alphabet = ["a", "b", "\r", "\n", "\x08", "é", "😀", "\x1b[0m", "xyz"];
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3000 {
            let mut buf = String::new();
            let mut t = String::new();
            let mut idx = 0;
            for _ in 0..(rnd() % 6) {
                let mut chunk = String::new();
                for _ in 0..(rnd() % 8) {
                    chunk.push_str(alphabet[(rnd() % alphabet.len() as u64) as usize]);
                }
                append(&mut buf, &chunk);
                let r = jlab(idx, &chunk, &t);
                t = r.0;
                idx = r.1;
                assert_eq!(visible(&buf), t, "{buf:?}");
            }
        }
    }

    #[test]
    fn truncate() {
        let text: String = (0..100_000).map(|i| format!("line {i}\n")).collect();
        let (shown, hidden) = truncate_for_display(&text, 1000, 1 << 20);
        assert_eq!(hidden, Some(99_000));
        assert_eq!(line_count(&shown), 1000);
        assert!(shown.ends_with("line 999"));
        assert!(matches!(shown, Cow::Borrowed(_)));

        let (shown, hidden) = truncate_for_display(&text, 1_000_000, 100);
        assert!(shown.len() <= 100 && shown.ends_with(char::is_numeric));
        assert_eq!(hidden.unwrap() + line_count(&shown), 100_000);

        let (shown, hidden) = truncate_for_display("short\ntext", 10, 100);
        assert_eq!((shown.as_ref(), hidden), ("short\ntext", None));

        let long_line = "x".repeat(1000) + "é";
        let (shown, hidden) = truncate_for_display(&long_line, 10, 1000);
        assert_eq!(shown.len(), 1000);
        assert_eq!(hidden, Some(1));

        let ht = head_tail(&text, 3, 2, 1 << 20).unwrap();
        assert_eq!(ht.head, "line 0\nline 1\nline 2");
        assert_eq!(ht.tail, "line 99998\nline 99999\n");
        assert_eq!(ht.hidden_lines, 100_000 - 5);
        assert!(head_tail("a\nb\n", 1, 1, 100).is_none());
    }

    #[test]
    fn line_counting() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("\n\n"), 2);
    }
}
