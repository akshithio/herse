//! What the reader sees of a frame: rows kept by filters and a search, in
//! a sorted order. [`compute_order`] runs off the main thread (a sort of a
//! million rows takes a few hundred milliseconds) and can be cancelled.

use std::{
    cmp::Ordering,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

use crate::{Frame, Kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Order {
    Ascending,
    Descending,
}

/// Sort, filters and search: everything that decides the visible rows.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ViewSpec {
    pub sort: Option<(usize, Order)>,
    pub filters: Vec<Filter>,
    /// Rows with this text in any column (case-insensitive).
    pub search: String,
}

impl ViewSpec {
    /// Every row, in file order.
    pub fn is_identity(&self) -> bool {
        self.sort.is_none() && self.filters.is_empty() && self.search.trim().is_empty()
    }
}

/// A condition on one column, as typed into its filter.
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub column: usize,
    /// What was typed, to show and to edit again.
    pub text: String,
    pub op: FilterOp,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FilterOp {
    /// Contains the text, ignoring case.
    Contains(String),
    NotContains(String),
    /// Is exactly the value (as a number, in a column of numbers).
    Equals(String),
    NotEquals(String),
    Less(Bound),
    LessOrEqual(Bound),
    Greater(Bound),
    GreaterOrEqual(Bound),
    /// `lo..hi` (both included) or `lo..<hi` (`hi` left out).
    Range { lo: Bound, hi: Bound, include_hi: bool },
    Null,
    NotNull,
}

/// The other side of a comparison: a number, a date or time (compared with
/// cells that parse as one, as seconds since 1970), or text compared as text.
#[derive(Clone, Debug, PartialEq)]
pub enum Bound {
    Number(f64),
    Time(f64),
    Text(String),
}

impl Filter {
    /// Read a filter: `text` (contains), `!text`, `=value`, `!=value`,
    /// `> 10`, `>= 10`, `< 10`, `<= 10`, `10..20`, `10..<20`, `null`,
    /// `!null`. Dates and times compare as times (`>= 2024-03-01`).
    pub fn parse(column: usize, input: &str) -> Result<Filter, String> {
        let text = input.trim();
        if text.is_empty() {
            return Err("Type a value to keep rows containing it.".into());
        }
        let lower = text.to_lowercase();
        let op = match lower.as_str() {
            "null" | "empty" | "is null" | "missing" => FilterOp::Null,
            "!null" | "not null" | "!empty" | "is not null" => FilterOp::NotNull,
            _ => {
                if let Some(rest) = text.strip_prefix(">=") {
                    FilterOp::GreaterOrEqual(bound(rest)?)
                } else if let Some(rest) = text.strip_prefix("<=") {
                    FilterOp::LessOrEqual(bound(rest)?)
                } else if let Some(rest) = text.strip_prefix("!=").or_else(|| text.strip_prefix("<>")) {
                    FilterOp::NotEquals(rest.trim().to_string())
                } else if let Some(rest) = text.strip_prefix("==").or_else(|| text.strip_prefix('=')) {
                    FilterOp::Equals(rest.trim().to_string())
                } else if let Some(rest) = text.strip_prefix('>') {
                    FilterOp::Greater(bound(rest)?)
                } else if let Some(rest) = text.strip_prefix('<') {
                    FilterOp::Less(bound(rest)?)
                } else if let Some(rest) = text.strip_prefix('!') {
                    FilterOp::NotContains(rest.trim().to_lowercase())
                } else if let Some((lo, hi)) = text.split_once("..")
                    && !lo.trim().is_empty()
                    && !hi.trim_start_matches('<').trim().is_empty()
                {
                    let (hi, include_hi) = match hi.strip_prefix('<') {
                        Some(hi) => (hi, false),
                        None => (hi, true),
                    };
                    FilterOp::Range { lo: bound(lo)?, hi: bound(hi)?, include_hi }
                } else {
                    FilterOp::Contains(lower)
                }
            }
        };
        Ok(Filter { column, text: text.to_string(), op })
    }

    /// Does row `row` pass?
    pub fn keeps(&self, frame: &Frame, kind: Kind, row: usize) -> bool {
        let c = self.column;
        let null = frame.is_null(row, c);
        let cell = || frame.text(row, c);
        let num = || frame.number(row, c);
        match &self.op {
            FilterOp::Null => null,
            FilterOp::NotNull => !null,
            FilterOp::Contains(needle) => !null && contains_ignore_case(cell(), needle),
            FilterOp::NotContains(needle) => null || !contains_ignore_case(cell(), needle),
            FilterOp::Equals(v) => !null && equals(cell(), num(), kind, v),
            FilterOp::NotEquals(v) => null || !equals(cell(), num(), kind, v),
            FilterOp::Less(b) => !null && compare(cell(), num(), b).is_some_and(Ordering::is_lt),
            FilterOp::LessOrEqual(b) => !null && compare(cell(), num(), b).is_some_and(Ordering::is_le),
            FilterOp::Greater(b) => !null && compare(cell(), num(), b).is_some_and(Ordering::is_gt),
            FilterOp::GreaterOrEqual(b) => !null && compare(cell(), num(), b).is_some_and(Ordering::is_ge),
            FilterOp::Range { lo, hi, include_hi } => {
                !null
                    && compare(cell(), num(), lo).is_some_and(Ordering::is_ge)
                    && compare(cell(), num(), hi).is_some_and(|o| if *include_hi { o.is_le() } else { o.is_lt() })
            }
        }
    }
}

fn bound(s: &str) -> Result<Bound, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Add a value after the comparison, like > 10.".into());
    }
    let s = s.trim_matches(['"', '\'']);
    Ok(match (s.parse::<f64>(), crate::profile::parse_time(s)) {
        (Ok(x), _) => Bound::Number(x),
        (_, Some(t)) => Bound::Time(t),
        _ => Bound::Text(s.to_string()),
    })
}

fn equals(cell: &str, num: Option<f64>, kind: Kind, v: &str) -> bool {
    if kind.is_numeric()
        && let (Some(x), Ok(want)) = (num, v.parse::<f64>())
    {
        return x == want;
    }
    cell.eq_ignore_ascii_case(v.trim_matches(['"', '\'']))
}

fn compare(cell: &str, num: Option<f64>, b: &Bound) -> Option<Ordering> {
    match b {
        Bound::Number(want) => num?.partial_cmp(want),
        Bound::Time(want) => crate::profile::parse_time(cell)?.partial_cmp(want),
        Bound::Text(want) => Some(cmp_text(cell, want)),
    }
}

/// `haystack` contains `needle` (already lowercase), ignoring case.
pub fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.is_ascii() {
        let n = needle.as_bytes();
        haystack.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        haystack.to_lowercase().contains(needle)
    }
}

/// Text order for people: ignoring case first, then by case.
fn cmp_text(a: &str, b: &str) -> Ordering {
    let ka = a.bytes().map(|c| c.to_ascii_lowercase());
    let kb = b.bytes().map(|c| c.to_ascii_lowercase());
    ka.cmp(kb).then_with(|| a.cmp(b))
}

/// Rows (in file order) whose text in any column contains `needle`,
/// ignoring case, as a bitset. `None` when cancelled.
pub fn search_hits(frame: &Frame, needle: &str, cancel: &AtomicBool) -> Option<Vec<u64>> {
    let needle = needle.trim().to_lowercase();
    let mut bits = vec![0u64; frame.rows().div_ceil(64)];
    for col in 0..frame.columns().len() {
        if cancel.load(AtomicOrdering::Relaxed) {
            return None;
        }
        for (start, cells, rows) in frame.column_chunks(col) {
            let Some(cells) = cells else { continue };
            if needle.is_ascii() {
                scan_ascii(&cells.text, &cells.ends, needle.as_bytes(), |r| bits[(start + r) / 64] |= 1 << ((start + r) % 64));
            } else {
                for r in 0..rows {
                    if cells.get(r).to_lowercase().contains(&needle) {
                        bits[(start + r) / 64] |= 1 << ((start + r) % 64);
                    }
                }
            }
        }
    }
    Some(bits)
}

/// Find `needle` (ASCII, lowercase) in a column's arena without visiting
/// every cell: jump between candidate first bytes with memchr, then map
/// each hit to its row.
fn scan_ascii(text: &str, ends: &[u32], needle: &[u8], mut hit: impl FnMut(usize)) {
    let hay = text.as_bytes();
    let Some(&first) = needle.first() else { return };
    let (lo, up) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
    let mut pos = 0;
    while pos < hay.len() {
        let found = if lo == up { memchr::memchr(lo, &hay[pos..]) } else { memchr::memchr2(lo, up, &hay[pos..]) };
        let Some(off) = found else { break };
        let at = pos + off;
        // The cell holding `at`: the first whose end is past it.
        let row = ends.partition_point(|&e| e as usize <= at);
        let Some(&end) = ends.get(row) else { break };
        let end = end as usize;
        if at + needle.len() <= end && hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            hit(row);
            pos = end;
        } else {
            pos = at + 1;
        }
    }
}

/// The visible rows for `spec`, in order; `None` when it shows every row in
/// file order, or when cancelled (check `cancel`).
pub fn compute_order(frame: &Frame, spec: &ViewSpec, cancel: &AtomicBool) -> Option<Vec<u32>> {
    if spec.is_identity() {
        return None;
    }
    let n = frame.rows();
    let mut rows: Vec<u32> = (0..n as u32).collect();
    if !spec.search.trim().is_empty() {
        let hits = search_hits(frame, &spec.search, cancel)?;
        rows.retain(|&r| hits[r as usize / 64] >> (r % 64) & 1 == 1);
    }
    for f in &spec.filters {
        if cancel.load(AtomicOrdering::Relaxed) {
            return None;
        }
        if f.column >= frame.columns().len() {
            continue;
        }
        let kind = frame.kind(f.column);
        rows.retain(|&r| f.keeps(frame, kind, r as usize));
    }
    if let Some((col, order)) = spec.sort
        && col < frame.columns().len()
    {
        if cancel.load(AtomicOrdering::Relaxed) {
            return None;
        }
        sort_rows(frame, col, order, &mut rows);
    }
    (!cancel.load(AtomicOrdering::Relaxed)).then_some(rows)
}

/// Sort by one column, missing values last either way, ties in file order.
fn sort_rows(frame: &Frame, col: usize, order: Order, rows: &mut [u32]) {
    let desc = order == Order::Descending;
    if frame.kind(col).is_numeric() {
        let mut keyed: Vec<(f64, u32)> =
            rows.iter().map(|&r| (frame.number(r as usize, col).unwrap_or(f64::NAN), r)).collect();
        keyed.sort_unstable_by(|a, b| match (a.0.is_nan(), b.0.is_nan()) {
            (true, true) => a.1.cmp(&b.1),
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => {
                let o = a.0.total_cmp(&b.0);
                (if desc { o.reverse() } else { o }).then(a.1.cmp(&b.1))
            }
        });
        for (slot, (_, r)) in rows.iter_mut().zip(keyed) {
            *slot = r;
        }
    } else {
        sort_text(frame, col, desc, rows);
    }
}

/// Text order for a column. Most text columns people sort are categories (a
/// few distinct values): rank the distinct values once and sort integers.
/// Otherwise compare an 8-byte, case-folded prefix first, and whole strings
/// only when prefixes tie.
fn sort_text(frame: &Frame, col: usize, desc: bool, rows: &mut [u32]) {
    let text = |r: u32| (!frame.is_null(r as usize, col)).then(|| frame.text(r as usize, col));
    let mut ranks: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let limit = (rows.len() / 8).max(64);
    let categorical = rows.iter().all(|&r| {
        if let Some(t) = text(r) {
            ranks.entry(t).or_insert(0);
        }
        ranks.len() <= limit
    });
    if categorical {
        let mut distinct: Vec<&str> = ranks.keys().copied().collect();
        distinct.sort_unstable_by(|a, b| cmp_text(a, b));
        if desc {
            distinct.reverse();
        }
        for (i, t) in distinct.into_iter().enumerate() {
            ranks.insert(t, i as u32);
        }
        // Missing values last, ties in file order: rank in the high bits.
        let mut keys: Vec<u64> = rows
            .iter()
            .map(|&r| (text(r).map_or(u32::MAX, |t| ranks[t]) as u64) << 32 | r as u64)
            .collect();
        keys.sort_unstable();
        for (slot, k) in rows.iter_mut().zip(keys) {
            *slot = k as u32;
        }
        return;
    }
    let prefix = |t: &str| {
        let mut k = [0u8; 8];
        for (d, s) in k.iter_mut().zip(t.bytes()) {
            *d = s.to_ascii_lowercase();
        }
        u64::from_be_bytes(k)
    };
    let mut keyed: Vec<(u64, Option<&str>, u32)> = rows
        .iter()
        .map(|&r| {
            let t = text(r);
            (t.map_or(0, prefix), t, r)
        })
        .collect();
    keyed.sort_unstable_by(|a, b| match (a.1, b.1) {
        (None, None) => a.2.cmp(&b.2),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let o = a.0.cmp(&b.0).then_with(|| cmp_text(x, y));
            (if desc { o.reverse() } else { o }).then(a.2.cmp(&b.2))
        }
    });
    for (slot, (_, _, r)) in rows.iter_mut().zip(keyed) {
        *slot = r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkBuilder, Column, Value};

    fn frame() -> Frame {
        let mut f = Frame::default();
        f.set_columns(vec![Column::new("city"), Column::new("people")]);
        let mut b = ChunkBuilder::new(2);
        for (city, n) in [("Oslo", "709000"), ("bergen", "291000"), ("Trondheim", ""), ("Ålesund", "67000")] {
            b.push_row([Value::Text(city), Value::Text(n)]);
        }
        f.push_chunk(b.finish());
        f
    }

    fn order(f: &Frame, spec: &ViewSpec) -> Vec<u32> {
        compute_order(f, spec, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn sorts_with_nulls_last() {
        let f = frame();
        let up = ViewSpec { sort: Some((1, Order::Ascending)), ..Default::default() };
        assert_eq!(order(&f, &up), [3, 1, 0, 2]);
        let down = ViewSpec { sort: Some((1, Order::Descending)), ..Default::default() };
        assert_eq!(order(&f, &down), [0, 1, 3, 2]);
        let by_name = ViewSpec { sort: Some((0, Order::Ascending)), ..Default::default() };
        assert_eq!(order(&f, &by_name), [1, 0, 2, 3], "case-insensitive; Å after ASCII");
    }

    #[test]
    fn filters() {
        let f = frame();
        let spec = |text: &str, col| ViewSpec { filters: vec![Filter::parse(col, text).unwrap()], ..Default::default() };
        assert_eq!(order(&f, &spec("> 100000", 1)), [0, 1]);
        assert_eq!(order(&f, &spec("60000..300000", 1)), [1, 3]);
        assert_eq!(order(&f, &spec("null", 1)), [2]);
        assert_eq!(order(&f, &spec("!null", 1)), [0, 1, 3]);
        assert_eq!(order(&f, &spec("O", 0)), [0, 2]);
        assert_eq!(order(&f, &spec("!o", 0)), [1, 3]);
        assert_eq!(order(&f, &spec("=oslo", 0)), [0]);
        assert_eq!(order(&f, &spec("=709000", 1)), [0]);
        assert_eq!(order(&f, &spec("ålesund", 0)), [3]);
        assert_eq!(order(&f, &spec("67000..<291000", 1)), [3], "half-open");
        assert_eq!(order(&f, &spec("67000..291000", 1)), [1, 3]);
        assert_eq!(order(&f, &spec("b..p", 0)), [0, 1], "text ranges");
        assert!(Filter::parse(0, ">").is_err());
    }

    #[test]
    fn search_spans_columns_and_ignores_case() {
        let f = frame();
        let spec = |s: &str| ViewSpec { search: s.into(), ..Default::default() };
        assert_eq!(order(&f, &spec("BERG")), [1]);
        assert_eq!(order(&f, &spec("000")), [0, 1, 3]);
        assert_eq!(order(&f, &spec("ÅLE")), [3]);
        assert_eq!(order(&f, &spec("zzz")), Vec::<u32>::new());
        // A hit must lie inside one cell, not across two.
        assert_eq!(order(&f, &spec("oslobergen")), Vec::<u32>::new());
    }

    #[test]
    fn time_ranges() {
        let mut f = Frame::default();
        f.set_columns(vec![Column::new("when")]);
        let mut b = ChunkBuilder::new(1);
        for t in ["2024-01-01 10:00", "2024-01-02T09:00", "2024-01-03 00:00"] {
            b.push_row([Value::Text(t)]);
        }
        f.push_chunk(b.finish());
        let spec = ViewSpec { filters: vec![Filter::parse(0, "2024-01-01 12:00..<2024-01-03").unwrap()], ..Default::default() };
        assert_eq!(order(&f, &spec), [1], "compared as times, whatever the separator");
    }

    #[test]
    fn identity_is_none() {
        assert!(compute_order(&frame(), &ViewSpec::default(), &AtomicBool::new(false)).is_none());
    }
}
