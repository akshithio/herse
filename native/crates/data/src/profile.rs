//! Column profiles: what a column holds, at a glance. Numbers (and ISO dates
//! and times written as text) become a histogram, text its most common
//! values, booleans a true/false split; every profile counts what's missing.
//!
//! [`profile`] works the bins or categories out from the values;
//! [`profile_like`] counts a subset of rows (a filtered view) into another
//! profile's bins, so the two can be drawn one over the other.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{Frame, Kind};

/// How many of a text column's values a profile names.
pub const TOP: usize = 8;
/// Past this many distinct values, new ones are no longer counted one by one.
const DISTINCT_CAP: usize = 100_000;
/// Bars a histogram aims for.
const BINS: f64 = 24.;

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// Rows looked at.
    pub count: usize,
    pub nulls: usize,
    pub shape: Shape,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Histogram(Histogram),
    Categories(Categories),
    Bools { yes: usize, no: usize },
    /// Nothing but missing values.
    Empty,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Histogram {
    /// `counts.len() + 1` edges: bin `i` holds `edges[i] <= x < edges[i + 1]`.
    pub edges: Vec<f64>,
    pub counts: Vec<usize>,
    /// The values are times read from text (seconds since 1970, UTC).
    pub time: Option<TimeKind>,
    /// Every value is a whole number: bins of one integer each when the
    /// range is small.
    pub integral: bool,
    /// The smallest and largest value (the edges are rounded past them).
    pub lo: f64,
    pub hi: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeKind {
    /// `2024-01-31`
    Date,
    /// `2024-01-31 13:45` (with or without seconds, `T`, a zone)
    DateTime,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Categories {
    /// The most common values, most common first (at most [`TOP`]).
    pub top: Vec<(String, usize)>,
    /// Values that aren't in `top`.
    pub other: usize,
    /// Distinct values (exact unless `capped`).
    pub distinct: usize,
    pub capped: bool,
}

impl Profile {
    /// Non-missing values.
    pub fn values(&self) -> usize {
        self.count - self.nulls
    }
}

impl Histogram {
    /// The bin holding `x`, if any.
    pub fn bin_of(&self, x: f64) -> Option<usize> {
        let n = self.counts.len();
        let (lo, hi) = (*self.edges.first()?, *self.edges.last()?);
        if !(x >= lo && x <= hi) {
            return None;
        }
        let ix = self.edges.partition_point(|&e| e <= x).saturating_sub(1);
        Some(ix.min(n - 1))
    }
}

/// A cell as profiles see it.
struct Cell<'a> {
    text: &'a str,
    null: bool,
    number: Option<f64>,
}

/// Visit column `col` at `rows` (every row when `None`), fast for the latter.
fn each<'f>(frame: &'f Frame, col: usize, rows: Option<&[u32]>, cancel: &AtomicBool, mut f: impl FnMut(Cell<'f>)) -> bool {
    match rows {
        None => {
            for (_, cells, n) in frame.column_chunks(col) {
                if cancel.load(Ordering::Relaxed) {
                    return false;
                }
                match cells {
                    Some(c) => {
                        for r in 0..n {
                            f(Cell { text: c.get(r), null: c.is_null(r), number: c.number(r) });
                        }
                    }
                    None => {
                        for _ in 0..n {
                            f(Cell { text: "", null: true, number: None });
                        }
                    }
                }
            }
        }
        Some(rows) => {
            for (i, &r) in rows.iter().enumerate() {
                if i % 65_536 == 0 && cancel.load(Ordering::Relaxed) {
                    return false;
                }
                let r = r as usize;
                f(Cell { text: frame.text(r, col), null: frame.is_null(r, col), number: frame.number(r, col) });
            }
        }
    }
    true
}

/// The profile of column `col` over `rows` (every row when `None`). `None`
/// when cancelled.
pub fn profile(frame: &Frame, col: usize, rows: Option<&[u32]>, cancel: &AtomicBool) -> Option<Profile> {
    let kind = frame.kind(col);
    let mut count = 0;
    let mut nulls = 0;
    let shape = match kind {
        Kind::Empty => {
            let done = each(frame, col, rows, cancel, |c| {
                count += 1;
                nulls += usize::from(c.null);
            });
            if !done {
                return None;
            }
            Shape::Empty
        }
        Kind::Int | Kind::Float => {
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            if rows.is_none() {
                let s = frame.stats(col);
                (lo, hi) = (s.min, s.max);
            } else if !each(frame, col, rows, cancel, |c| {
                if let Some(x) = c.number.filter(|x| x.is_finite()) {
                    lo = lo.min(x);
                    hi = hi.max(x);
                }
            }) {
                return None;
            }
            let edges = number_edges(lo, hi, kind == Kind::Int);
            let mut h = Histogram { counts: vec![0; edges.len().saturating_sub(1)], edges, time: None, integral: kind == Kind::Int, lo, hi };
            let done = each(frame, col, rows, cancel, |c| {
                count += 1;
                if c.null {
                    nulls += 1;
                } else if let Some(b) = c.number.and_then(|x| h.bin_of(x)) {
                    h.counts[b] += 1;
                }
            });
            if !done {
                return None;
            }
            if h.counts.is_empty() { Shape::Empty } else { Shape::Histogram(h) }
        }
        Kind::Bool => {
            let (mut yes, mut no) = (0, 0);
            let done = each(frame, col, rows, cancel, |c| {
                count += 1;
                if c.null {
                    nulls += 1;
                } else if crate::parse_bool(c.text) == Some(true) {
                    yes += 1;
                } else {
                    no += 1;
                }
            });
            if !done {
                return None;
            }
            Shape::Bools { yes, no }
        }
        Kind::Text => match time_kind(frame, col, rows) {
            Some(time) => {
                let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
                if !each(frame, col, rows, cancel, |c| {
                    if let Some(t) = (!c.null).then(|| parse_time(c.text)).flatten() {
                        lo = lo.min(t);
                        hi = hi.max(t);
                    }
                }) {
                    return None;
                }
                let edges = time_edges(lo, hi);
                let mut h = Histogram { counts: vec![0; edges.len().saturating_sub(1)], edges, time: Some(time), integral: false, lo, hi };
                let done = each(frame, col, rows, cancel, |c| {
                    count += 1;
                    if c.null {
                        nulls += 1;
                    } else if let Some(b) = parse_time(c.text).and_then(|t| h.bin_of(t)) {
                        h.counts[b] += 1;
                    }
                });
                if !done {
                    return None;
                }
                if h.counts.is_empty() { Shape::Empty } else { Shape::Histogram(h) }
            }
            None => {
                let mut seen: HashMap<&str, usize> = HashMap::new();
                let mut uncounted = 0;
                let done = each(frame, col, rows, cancel, |c| {
                    count += 1;
                    if c.null {
                        nulls += 1;
                    } else if let Some(n) = seen.get_mut(c.text) {
                        *n += 1;
                    } else if seen.len() < DISTINCT_CAP {
                        seen.insert(c.text, 1);
                    } else {
                        uncounted += 1;
                    }
                });
                if !done {
                    return None;
                }
                let distinct = seen.len();
                let mut all: Vec<(&str, usize)> = seen.into_iter().collect();
                // Most common first; ties in text order, so it's stable.
                all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
                let top: Vec<(String, usize)> = all.iter().take(TOP).map(|(t, n)| (t.to_string(), *n)).collect();
                let shown: usize = top.iter().map(|t| t.1).sum();
                Shape::Categories(Categories {
                    other: count - nulls - shown,
                    top,
                    distinct,
                    capped: uncounted > 0,
                })
            }
        },
    };
    Some(Profile { count, nulls, shape })
}

/// `rows` counted into `like`'s bins or categories. `None` when cancelled.
pub fn profile_like(frame: &Frame, col: usize, rows: &[u32], like: &Profile, cancel: &AtomicBool) -> Option<Profile> {
    let mut count = 0;
    let mut nulls = 0;
    let shape = match &like.shape {
        Shape::Empty => {
            if !each(frame, col, Some(rows), cancel, |c| {
                count += 1;
                nulls += usize::from(c.null);
            }) {
                return None;
            }
            Shape::Empty
        }
        Shape::Bools { .. } => {
            let (mut yes, mut no) = (0, 0);
            if !each(frame, col, Some(rows), cancel, |c| {
                count += 1;
                if c.null {
                    nulls += 1;
                } else if crate::parse_bool(c.text) == Some(true) {
                    yes += 1;
                } else {
                    no += 1;
                }
            }) {
                return None;
            }
            Shape::Bools { yes, no }
        }
        Shape::Histogram(base) => {
            let mut h = Histogram { counts: vec![0; base.counts.len()], ..base.clone() };
            let time = base.time.is_some();
            if !each(frame, col, Some(rows), cancel, |c| {
                count += 1;
                if c.null {
                    nulls += 1;
                    return;
                }
                let x = if time { parse_time(c.text) } else { c.number };
                if let Some(b) = x.and_then(|x| h.bin_of(x)) {
                    h.counts[b] += 1;
                }
            }) {
                return None;
            }
            Shape::Histogram(h)
        }
        Shape::Categories(base) => {
            let index: HashMap<&str, usize> = base.top.iter().enumerate().map(|(i, (t, _))| (t.as_str(), i)).collect();
            let mut counts = vec![0; base.top.len()];
            let mut other = 0;
            if !each(frame, col, Some(rows), cancel, |c| {
                count += 1;
                if c.null {
                    nulls += 1;
                } else if let Some(&i) = index.get(c.text) {
                    counts[i] += 1;
                } else {
                    other += 1;
                }
            }) {
                return None;
            }
            Shape::Categories(Categories {
                top: base.top.iter().zip(counts).map(|((t, _), n)| (t.clone(), n)).collect(),
                other,
                distinct: base.distinct,
                capped: base.capped,
            })
        }
    };
    Some(Profile { count, nulls, shape })
}

/// Bin edges for numbers: one bin per integer for small whole ranges, else
/// about two dozen bins on round boundaries (1, 2 or 5 × a power of ten).
fn number_edges(lo: f64, hi: f64, integral: bool) -> Vec<f64> {
    if !(lo.is_finite() && hi.is_finite()) || lo > hi {
        return Vec::new();
    }
    if integral && hi - lo <= 40. {
        return (lo as i64..=hi as i64 + 1).map(|v| v as f64).collect();
    }
    if hi == lo {
        return vec![lo, lo + 1.];
    }
    let raw = (hi - lo) / BINS;
    let pow = 10f64.powf(raw.log10().floor());
    let step = [1., 2., 5., 10.].iter().map(|m| m * pow).find(|s| *s >= raw).unwrap_or(10. * pow);
    let step = if integral { step.max(1.).ceil() } else { step };
    let start = (lo / step).floor() * step;
    let n = ((hi - start) / step).floor() as usize + 1;
    (0..=n).map(|k| start + k as f64 * step).collect()
}

/// Bin edges for times: about two dozen bins of a round duration.
fn time_edges(lo: f64, hi: f64) -> Vec<f64> {
    if !(lo.is_finite() && hi.is_finite()) || lo > hi {
        return Vec::new();
    }
    const STEPS: &[f64] = &[
        1., 5., 15., 30., 60., 300., 900., 1800., 3600., 3. * 3600., 6. * 3600., 12. * 3600., 86_400., 2. * 86_400.,
        7. * 86_400., 14. * 86_400., 30. * 86_400., 91. * 86_400., 182. * 86_400., 365. * 86_400.,
    ];
    if hi == lo {
        return vec![lo, lo + 86_400.];
    }
    let raw = (hi - lo) / BINS;
    let step = STEPS.iter().copied().find(|s| *s >= raw).unwrap_or_else(|| (raw / (365. * 86_400.)).ceil() * 365. * 86_400.);
    let start = (lo / step).floor() * step;
    let n = ((hi - start) / step).floor() as usize + 1;
    (0..=n).map(|k| start + k as f64 * step).collect()
}

/// Is column `col` (text) a column of ISO dates or times? Looks at up to
/// 200 values; every one must parse.
fn time_kind(frame: &Frame, col: usize, rows: Option<&[u32]>) -> Option<TimeKind> {
    let mut kind = TimeKind::Date;
    let mut seen = 0;
    let n = rows.map_or(frame.rows(), <[u32]>::len);
    for i in 0..n {
        let r = rows.map_or(i, |rows| rows[i] as usize);
        if frame.is_null(r, col) {
            continue;
        }
        let text = frame.text(r, col);
        parse_time(text)?;
        if text.len() > 10 {
            kind = TimeKind::DateTime;
        }
        seen += 1;
        if seen == 200 {
            break;
        }
    }
    (seen > 0).then_some(kind)
}

/// An ISO date or date-time as seconds since 1970 (UTC; an offset is
/// applied): `2024-01-31`, `2024-01-31 13:45`, `2024-01-31T13:45:07.25Z`,
/// `2024-01-31 13:45:07+02:00`.
pub fn parse_time(text: &str) -> Option<f64> {
    let b = text.trim().as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |s: &[u8]| -> Option<i64> {
        if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(s.iter().fold(0i64, |n, d| n * 10 + (d - b'0') as i64))
    };
    let (y, m, d) = (num(&b[0..4])?, num(&b[5..7])?, num(&b[8..10])?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m, d) as f64 * 86_400.;
    let rest = &b[10..];
    if rest.is_empty() {
        return Some(secs);
    }
    if !(rest[0] == b' ' || rest[0] == b'T') || rest.len() < 6 || rest[3] != b':' {
        return None;
    }
    let (h, mi) = (num(&rest[1..3])?, num(&rest[4..6])?);
    if h > 23 || mi > 59 {
        return None;
    }
    secs += (h * 3600 + mi * 60) as f64;
    let mut rest = &rest[6..];
    if rest.first() == Some(&b':') {
        let s = num(rest.get(1..3)?)?;
        if s > 60 {
            return None;
        }
        secs += s as f64;
        rest = &rest[3..];
        if rest.first() == Some(&b'.') {
            let digits = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 {
                return None;
            }
            let frac: f64 = std::str::from_utf8(&rest[..digits + 1]).ok()?.parse().ok()?;
            secs += frac;
            rest = &rest[digits + 1..];
        }
    }
    match rest {
        [] | [b'Z'] => Some(secs),
        [sign @ (b'+' | b'-'), zone @ ..] if zone.len() == 5 && zone[2] == b':' => {
            let off = (num(&zone[0..2])? * 3600 + num(&zone[3..5])? * 60) as f64;
            Some(if *sign == b'+' { secs - off } else { secs + off })
        }
        _ => None,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A time from [`parse_time`] as text: `2024-01-31` or `2024-01-31 13:45`.
pub fn format_time(secs: f64, kind: TimeKind) -> String {
    let days = (secs / 86_400.).floor() as i64;
    let rem = secs - days as f64 * 86_400.;
    // Inverse of days_from_civil.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    match kind {
        TimeKind::Date => format!("{y:04}-{m:02}-{d:02}"),
        TimeKind::DateTime => {
            let mins = ((rem / 60.).floor() as i64).min(24 * 60 - 1);
            format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", mins / 60, mins % 60)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkBuilder, Column, Value};

    fn frame(cols: &[&str], rows: &[&[&str]]) -> Frame {
        let mut f = Frame::default();
        f.set_columns(cols.iter().map(|c| Column::new(*c)).collect());
        let mut b = ChunkBuilder::new(cols.len());
        for row in rows {
            b.push_row(row.iter().map(|t| Value::Text(t)));
        }
        f.push_chunk(b.finish());
        f
    }

    fn go() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn small_integers_get_a_bar_each() {
        let f = frame(&["n"], &[&["1"], &["2"], &["2"], &["5"], &[""]]);
        let p = profile(&f, 0, None, &go()).unwrap();
        assert_eq!((p.count, p.nulls), (5, 1));
        let Shape::Histogram(h) = p.shape else { panic!() };
        assert_eq!(h.edges, [1., 2., 3., 4., 5., 6.]);
        assert_eq!(h.counts, [1, 2, 0, 0, 1]);
    }

    #[test]
    fn floats_get_round_bins() {
        let rows: Vec<String> = (0..1000).map(|i| format!("{}", i as f64 * 0.37)).collect();
        let refs: Vec<[&str; 1]> = rows.iter().map(|r| [r.as_str()]).collect();
        let rows: Vec<&[&str]> = refs.iter().map(|r| &r[..]).collect();
        let f = frame(&["x"], &rows);
        let Shape::Histogram(h) = profile(&f, 0, None, &go()).unwrap().shape else { panic!() };
        assert_eq!(h.edges[0], 0.);
        assert_eq!(h.edges[1], 20., "a round step");
        assert!(*h.edges.last().unwrap() >= 369.63);
        assert_eq!(h.counts.iter().sum::<usize>(), 1000);
    }

    #[test]
    fn text_gets_top_values() {
        let f = frame(&["c"], &[&["b"], &["a"], &["b"], &["c"], &["b"], &["NA"]]);
        let p = profile(&f, 0, None, &go()).unwrap();
        let Shape::Categories(c) = &p.shape else { panic!() };
        assert_eq!(c.top[0], ("b".to_string(), 3));
        assert_eq!(c.top[1], ("a".to_string(), 1));
        assert_eq!((c.distinct, c.other, p.nulls), (3, 0, 1));
        // A subset counted into the same categories.
        let sub = profile_like(&f, 0, &[0, 1], &p, &go()).unwrap();
        let Shape::Categories(s) = sub.shape else { panic!() };
        assert_eq!(s.top.iter().map(|t| t.1).collect::<Vec<_>>(), [1, 1, 0]);
    }

    #[test]
    fn dates_in_text_get_a_histogram() {
        let f = frame(&["d"], &[&["2024-01-01"], &["2024-01-15"], &["2024-03-01"]]);
        let Shape::Histogram(h) = profile(&f, 0, None, &go()).unwrap().shape else { panic!() };
        assert_eq!(h.time, Some(TimeKind::Date));
        assert_eq!(h.counts.iter().sum::<usize>(), 3);
        assert_eq!(format_time(h.edges[0], TimeKind::Date), "2023-12-28", "a whole-week step");
    }

    #[test]
    fn bools() {
        let f = frame(&["b"], &[&["True"], &["False"], &["True"]]);
        assert_eq!(profile(&f, 0, None, &go()).unwrap().shape, Shape::Bools { yes: 2, no: 1 });
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("1970-01-01"), Some(0.));
        assert_eq!(parse_time("1970-01-02 00:01"), Some(86_460.));
        assert_eq!(parse_time("2000-03-01T00:00:00Z"), Some(951_868_800.));
        assert_eq!(parse_time("2000-03-01T02:00:00+02:00"), Some(951_868_800.));
        assert_eq!(parse_time("2000-03-01 00:00:01.5"), Some(951_868_801.5));
        for bad in ["2024-13-01", "2024/01/01", "20240101", "2024-01-01 25:00", "2024-01-01x", "hello world"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(format_time(951_868_800., TimeKind::DateTime), "2000-03-01 00:00");
        assert_eq!(format_time(-86_400., TimeKind::Date), "1969-12-31");
    }
}
