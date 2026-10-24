//! Tick generation: plotly.js's auto ticks and d3's (Vega's) ticks / nice.

use crate::time::{self, DAY, HOUR, MINUTE, SECOND};

// ---------------------------------------------------------------- plotly

/// plotly `Lib.roundUp`: first value in `set` >= v (last if none).
fn round_up(v: f64, set: &[f64]) -> f64 {
    for &s in set {
        if s >= v {
            return s;
        }
    }
    set[set.len() - 1]
}

const ROUND_BASE10: [f64; 3] = [2., 5., 10.];
const ROUND_BASE24: [f64; 5] = [1., 2., 3., 6., 12.];
const ROUND_BASE60: [f64; 6] = [1., 2., 5., 10., 15., 30.];
const ROUND_DAYS: [f64; 5] = [1., 2., 3., 7., 14.];

fn round_dtick(rough: f64, base: f64, set: &[f64]) -> f64 {
    base * round_up(rough / base, set)
}

fn base10(rough: f64) -> f64 {
    10f64.powf(rough.log10().floor())
}

/// plotly `autoTicks` for a linear axis.
pub fn plotly_linear_dtick(rough: f64) -> f64 {
    if !(rough.is_finite() && rough > 0.) {
        return 1.;
    }
    round_dtick(rough, base10(rough), &ROUND_BASE10)
}

/// Number of ticks plotly aims for on an axis `len` px long.
pub fn plotly_nticks(len: f64, is_y: bool, nticks: Option<f64>) -> f64 {
    if let Some(n) = nticks.filter(|n| *n > 0.) {
        return n;
    }
    let min_px = if is_y { 40. } else { 80. };
    (len / min_px).clamp(4., 9.) + 1.
}

/// Linear tick values in [lo, hi] (either order) for `dtick` from `tick0`.
pub fn linear_ticks(r0: f64, r1: f64, dtick: f64, tick0: f64, max: usize) -> Vec<f64> {
    let (lo, hi) = if r0 <= r1 { (r0, r1) } else { (r1, r0) };
    if !(dtick > 0. && lo.is_finite() && hi.is_finite()) {
        return Vec::new();
    }
    // plotly extends the range by 1e-4 so end ticks lost to rounding stay.
    let eps = (hi - lo).abs() * 1e-4;
    let first = ((lo - eps - tick0) / dtick).ceil();
    let last = ((hi + eps - tick0) / dtick).floor();
    let count = last - first;
    if !(first.is_finite() && last.is_finite()) || count.is_nan() || count < 0. || count > max as f64 {
        return Vec::new();
    }
    // Count with an integer: `k += 1.` stalls once k is huge.
    (0..=count as usize)
        .map(|i| {
            let v = tick0 + (first + i as f64) * dtick;
            // Clean binary noise (0.30000000000000004 -> 0.3).
            clean(v, dtick)
        })
        .collect()
}

fn clean(v: f64, step: f64) -> f64 {
    if v.abs() < step * 1e-8 {
        return 0.;
    }
    let digits = (-(step.abs().log10().floor()) + 10.).clamp(0., 20.) as i32;
    let p = 10f64.powi(digits);
    let r = (v * p).round() / p;
    if r.is_finite() { r } else { v }
}

/// A plotly date-axis tick spacing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DateDtick {
    Ms(f64),
    Months(i64),
}

/// plotly `autoTicks` for a date axis; returns (dtick, tick0 ms).
pub fn plotly_date_dtick(rough: f64) -> (DateDtick, f64) {
    let tick0 = time::ms_from_ymd(2000, 1, 1);
    let rough = if rough.is_finite() && rough > 0. { rough } else { DAY };
    let x2 = 2. * rough;
    if x2 > time::AVG_YEAR {
        let r = rough / time::AVG_YEAR;
        let years = round_dtick(r, base10(r), &ROUND_BASE10);
        (DateDtick::Months((12. * years).round().max(12.) as i64), tick0)
    } else if x2 > time::AVG_MONTH {
        let r = rough / time::AVG_MONTH;
        (DateDtick::Months(round_dtick(r, 1., &ROUND_BASE24) as i64), tick0)
    } else if x2 > DAY {
        let d = round_dtick(rough, DAY, &ROUND_DAYS);
        // Weekly ticks land on Sundays.
        (DateDtick::Ms(d), time::ms_from_ymd(2000, 1, 2))
    } else if x2 > HOUR {
        (DateDtick::Ms(round_dtick(rough, HOUR, &ROUND_BASE24)), tick0)
    } else if x2 > MINUTE {
        (DateDtick::Ms(round_dtick(rough, MINUTE, &ROUND_BASE60)), tick0)
    } else if x2 > SECOND {
        (DateDtick::Ms(round_dtick(rough, SECOND, &ROUND_BASE60)), tick0)
    } else {
        (DateDtick::Ms(round_dtick(rough, base10(rough), &ROUND_BASE10).max(0.1)), tick0)
    }
}

/// Date tick values within [lo, hi].
pub fn date_ticks(lo: f64, hi: f64, dtick: DateDtick, tick0: f64, max: usize) -> Vec<f64> {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    match dtick {
        DateDtick::Ms(d) => linear_ticks(lo, hi, d, tick0, max),
        DateDtick::Months(n) => {
            if n <= 0 || !lo.is_finite() || !hi.is_finite() {
                return Vec::new();
            }
            let p0 = time::parts(tick0);
            let base = p0.year * 12 + (p0.month - 1);
            let pl = time::parts(lo);
            let mut m = pl.year * 12 + (pl.month - 1);
            m -= (m - base).rem_euclid(n);
            let mut out = Vec::new();
            let eps = 1.;
            loop {
                let t = time::ms_from_ymd(m.div_euclid(12), m.rem_euclid(12) + 1, p0.day);
                if t > hi + eps || out.len() > max {
                    break;
                }
                if t >= lo - eps {
                    out.push(t);
                }
                let Some(next) = m.checked_add(n) else { break };
                m = next;
            }
            out
        }
    }
}

/// How plotly rounds date tick labels (`ax._tickround`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateRound {
    Year,
    Month,
    Day,
    Minute,
    Second,
    /// Fractional-second digits.
    Frac(u8),
}

pub fn plotly_date_round(dtick: DateDtick, tick0: f64) -> DateRound {
    // tick0 '2000-01-01' / '2000-01-02' are 10 characters: date only.
    let tick0_has_time = time::parts(tick0).hour != 0;
    match dtick {
        DateDtick::Months(n) => {
            let p = time::parts(tick0);
            if tick0_has_time || p.month != 1 || p.day != 1 {
                DateRound::Day
            } else if n % 12 == 0 {
                DateRound::Year
            } else {
                DateRound::Month
            }
        }
        DateDtick::Ms(d) => {
            if (d >= DAY && !tick0_has_time) || d >= 15. * DAY {
                DateRound::Day
            } else if d >= MINUTE || d >= HOUR {
                DateRound::Minute
            } else if d >= SECOND {
                DateRound::Second
            } else {
                let digits = (-(d / 1000.).log10()).ceil().clamp(1., 4.) as u8;
                DateRound::Frac(digits)
            }
        }
    }
}

/// plotly date tick label: (main line, optional "head" second line).
pub fn plotly_date_label(t: f64, round: DateRound) -> (String, Option<String>) {
    match round {
        DateRound::Year => (time::strftime("%Y", t), None),
        DateRound::Month => (time::strftime("%b %Y", t), None),
        DateRound::Day => (time::strftime("%b %-d", t), Some(time::strftime("%Y", t))),
        DateRound::Minute => (time::strftime("%H:%M", t), Some(time::strftime("%b %-d, %Y", t))),
        DateRound::Second => (time::strftime("%H:%M:%S", t), Some(time::strftime("%b %-d, %Y", t))),
        DateRound::Frac(n) => {
            let mut s = time::strftime("%H:%M:%S", t);
            let p = time::parts(t);
            let frac = format!("{:.*}", n as usize, p.ms / 1000.);
            let frac = frac.get(1..).unwrap_or("").trim_end_matches('0').trim_end_matches('.').to_string();
            s.push_str(&frac);
            (s, Some(time::strftime("%b %-d, %Y", t)))
        }
    }
}

/// plotly log axis tick spacing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogDtick {
    /// Every n decades.
    Decades(f64),
    /// 1, 2, 5 within each decade.
    D2,
    /// Every digit.
    D1,
    /// Linear spacing (in value space) for spans under a decade.
    L(f64),
}

/// plotly `autoTicks` for a log axis with range `r` (log10 units).
pub fn plotly_log_dtick(r: (f64, f64), rough: f64) -> LogDtick {
    if rough > 0.7 {
        LogDtick::Decades(rough.ceil())
    } else if (r.1 - r.0).abs() < 1. {
        let nt = 1.5 * ((r.1 - r.0) / rough).abs();
        let rough_v = (10f64.powf(r.1) - 10f64.powf(r.0)).abs() / nt;
        LogDtick::L(plotly_linear_dtick(rough_v))
    } else if rough > 0.3 {
        LogDtick::D2
    } else {
        LogDtick::D1
    }
}

/// Log tick positions (log10 units) within the range.
pub fn log_ticks(r: (f64, f64), dtick: LogDtick, max: usize) -> Vec<f64> {
    let (lo, hi) = if r.0 <= r.1 { r } else { (r.1, r.0) };
    if !(lo.is_finite() && hi.is_finite()) || hi - lo > 400. {
        return Vec::new();
    }
    match dtick {
        LogDtick::Decades(n) => linear_ticks(lo, hi, n.max(1.), 0., max),
        LogDtick::L(d) => linear_ticks(10f64.powf(lo), 10f64.powf(hi), d, 0., max)
            .into_iter()
            .filter(|v| *v > 0.)
            .map(f64::log10)
            .collect(),
        LogDtick::D1 | LogDtick::D2 => {
            let digits: &[f64] = if dtick == LogDtick::D1 {
                &[1., 2., 3., 4., 5., 6., 7., 8., 9.]
            } else {
                &[1., 2., 5.]
            };
            let mut out = Vec::new();
            let decades = (hi.ceil() - lo.floor()).max(0.) as usize;
            for i in 0..=decades.min(max) {
                let dec = lo.floor() + i as f64;
                for d in digits {
                    let v = dec + d.log10();
                    if v >= lo - 1e-9 && v <= hi + 1e-9 {
                        out.push(v);
                    }
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------- d3 / vega

const E10: f64 = 7.0710678118654755; // sqrt(50)
const E5: f64 = 3.1622776601683795; // sqrt(10)
const E2: f64 = std::f64::consts::SQRT_2;

fn tick_spec(start: f64, stop: f64, count: f64) -> (f64, f64, f64) {
    let step = (stop - start) / count.max(0.);
    let power = step.log10().floor();
    let error = step / 10f64.powf(power);
    let factor = if error >= E10 {
        10.
    } else if error >= E5 {
        5.
    } else if error >= E2 {
        2.
    } else {
        1.
    };
    let (mut i1, mut i2, inc);
    if power < 0. {
        let inc0 = 10f64.powf(-power) / factor;
        i1 = (start * inc0).round();
        i2 = (stop * inc0).round();
        if i1 / inc0 < start {
            i1 += 1.;
        }
        if i2 / inc0 > stop {
            i2 -= 1.;
        }
        inc = -inc0;
    } else {
        let inc0 = 10f64.powf(power) * factor;
        i1 = (start / inc0).round();
        i2 = (stop / inc0).round();
        if i1 * inc0 < start {
            i1 += 1.;
        }
        if i2 * inc0 > stop {
            i2 -= 1.;
        }
        inc = inc0;
    }
    if i2 < i1 && (0.5..2.).contains(&count) {
        return tick_spec(start, stop, count * 2.);
    }
    (i1, i2, inc)
}

/// d3 `ticks(start, stop, count)`.
pub fn d3_ticks(start: f64, stop: f64, count: f64) -> Vec<f64> {
    if count.is_nan() || count <= 0. || !start.is_finite() || !stop.is_finite() {
        return Vec::new();
    }
    if start == stop {
        return vec![start];
    }
    let reverse = stop < start;
    let (a, b) = if reverse { (stop, start) } else { (start, stop) };
    let (i1, i2, inc) = tick_spec(a, b, count);
    if i1.is_nan() || i2.is_nan() || i2 < i1 || i2 - i1 > 10000. || inc == 0. || !inc.is_finite() {
        return Vec::new();
    }
    let n = (i2 - i1 + 1.) as usize;
    let mut out: Vec<f64> = (0..n)
        .map(|i| {
            let k = i1 + i as f64;
            if inc < 0. { k / -inc } else { k * inc }
        })
        .collect();
    if reverse {
        out.reverse();
    }
    out
}

/// d3 `tickIncrement`.
pub fn d3_tick_increment(start: f64, stop: f64, count: f64) -> f64 {
    tick_spec(start, stop, count).2
}

/// d3 `tickStep`.
pub fn d3_tick_step(start: f64, stop: f64, count: f64) -> f64 {
    let reverse = stop < start;
    let inc = if reverse { d3_tick_increment(stop, start, count) } else { d3_tick_increment(start, stop, count) };
    (if reverse { -1. } else { 1. }) * if inc < 0. { 1. / -inc } else { inc }
}

/// d3 linear `nice(count)`.
pub fn d3_nice(start: f64, stop: f64, count: f64) -> (f64, f64) {
    let (mut a, mut b) = (start, stop);
    let reverse = b < a;
    if reverse {
        std::mem::swap(&mut a, &mut b);
    }
    if !(a.is_finite() && b.is_finite()) || a == b {
        return (start, stop);
    }
    let mut prestep = f64::NAN;
    for _ in 0..10 {
        let step = d3_tick_increment(a, b, count);
        if step == prestep {
            break;
        } else if step > 0. {
            a = (a / step).floor() * step;
            b = (b / step).ceil() * step;
        } else if step < 0. {
            a = (a * step).ceil() / step;
            b = (b * step).floor() / step;
        } else {
            break;
        }
        prestep = step;
    }
    if reverse { (b, a) } else { (a, b) }
}

/// Precision (fraction digits) d3 uses for ticks of `step` (precisionFixed).
pub fn d3_precision_fixed(step: f64) -> usize {
    let step = step.abs();
    if step.is_nan() || step <= 0. || !step.is_finite() {
        return 0;
    }
    // exponent of step in toExponential form
    let e = format!("{step:e}");
    let exp: i32 = e.split_once('e').and_then(|(_, x)| x.parse().ok()).unwrap_or(0);
    (-exp).max(0) as usize
}

/// d3 log-scale ticks for a positive domain (base 10).
pub fn d3_log_ticks(u: f64, v: f64, count: f64) -> Vec<f64> {
    let (u, v, r) = if v < u { (v, u, true) } else { (u, v, false) };
    if !(u > 0. && v.is_finite()) {
        return Vec::new();
    }
    let i0 = u.log10();
    let j0 = v.log10();
    let n = if count > 0. { count } else { 10. };
    let mut z = Vec::new();
    if j0 - i0 < n {
        let mut i = i0.floor();
        let j = j0.ceil();
        while i <= j && z.len() < 2000 {
            for k in 1..10 {
                let t = if i < 0. { k as f64 / 10f64.powf(-i) } else { k as f64 * 10f64.powf(i) };
                if t < u {
                    continue;
                }
                if t > v {
                    break;
                }
                z.push(t);
            }
            i += 1.;
        }
        if (z.len() as f64) * 2. < n {
            z = d3_ticks(u, v, n);
        }
    } else {
        z = d3_ticks(i0, j0, (j0 - i0).min(n)).into_iter().map(|e| 10f64.powf(e)).collect();
    }
    if r {
        z.reverse();
    }
    z
}

/// d3 time-scale tick intervals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeUnit {
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

const TICK_INTERVALS: [(TimeUnit, i64, f64); 18] = [
    (TimeUnit::Second, 1, SECOND),
    (TimeUnit::Second, 5, 5. * SECOND),
    (TimeUnit::Second, 15, 15. * SECOND),
    (TimeUnit::Second, 30, 30. * SECOND),
    (TimeUnit::Minute, 1, MINUTE),
    (TimeUnit::Minute, 5, 5. * MINUTE),
    (TimeUnit::Minute, 15, 15. * MINUTE),
    (TimeUnit::Minute, 30, 30. * MINUTE),
    (TimeUnit::Hour, 1, HOUR),
    (TimeUnit::Hour, 3, 3. * HOUR),
    (TimeUnit::Hour, 6, 6. * HOUR),
    (TimeUnit::Hour, 12, 12. * HOUR),
    (TimeUnit::Day, 1, DAY),
    (TimeUnit::Day, 2, 2. * DAY),
    (TimeUnit::Week, 1, 7. * DAY),
    (TimeUnit::Month, 1, 30. * DAY),
    (TimeUnit::Month, 3, 90. * DAY),
    (TimeUnit::Year, 1, 365. * DAY),
];

/// d3 `tickInterval(start, stop, count)`.
pub fn d3_time_interval(start: f64, stop: f64, count: f64) -> (TimeUnit, i64) {
    let target = (stop - start).abs() / count.max(1e-9);
    let i = TICK_INTERVALS.partition_point(|&(_, _, step)| step <= target);
    if i == TICK_INTERVALS.len() {
        let y = d3_tick_step(start / (365. * DAY), stop / (365. * DAY), count).max(1.);
        return (TimeUnit::Year, y.round().max(1.) as i64);
    }
    if i == 0 {
        return (TimeUnit::Millisecond, d3_tick_step(start, stop, count).max(1.).round() as i64);
    }
    let (u0, s0, d0) = TICK_INTERVALS[i - 1];
    let (u1, s1, d1) = TICK_INTERVALS[i];
    if target / d0 < d1 / target { (u0, s0) } else { (u1, s1) }
}

/// Floors `t` to the unit (UTC).
pub fn time_floor(t: f64, unit: TimeUnit) -> f64 {
    let p = time::parts(t);
    match unit {
        TimeUnit::Millisecond => t.floor(),
        TimeUnit::Second => (t / SECOND).floor() * SECOND,
        TimeUnit::Minute => (t / MINUTE).floor() * MINUTE,
        TimeUnit::Hour => (t / HOUR).floor() * HOUR,
        TimeUnit::Day => (t / DAY).floor() * DAY,
        TimeUnit::Week => {
            let d = (t / DAY).floor();
            (d - p.weekday as f64) * DAY
        }
        TimeUnit::Month => time::ms_from_ymd(p.year, p.month, 1),
        TimeUnit::Year => time::ms_from_ymd(p.year, 1, 1),
    }
}

fn time_offset(t: f64, unit: TimeUnit, n: i64) -> f64 {
    match unit {
        TimeUnit::Millisecond => t + n as f64,
        TimeUnit::Second => t + n as f64 * SECOND,
        TimeUnit::Minute => t + n as f64 * MINUTE,
        TimeUnit::Hour => t + n as f64 * HOUR,
        TimeUnit::Day => t + n as f64 * DAY,
        TimeUnit::Week => t + n as f64 * 7. * DAY,
        TimeUnit::Month => time::add_months(t, n),
        TimeUnit::Year => time::add_months(t, 12 * n),
    }
}

/// `interval.every(step)` filter: does `t` (already floored) belong?
fn time_every_ok(t: f64, unit: TimeUnit, step: i64) -> bool {
    if step <= 1 {
        return true;
    }
    let p = time::parts(t);
    match unit {
        TimeUnit::Millisecond => (t as i64).rem_euclid(step) == 0,
        TimeUnit::Second => p.second % step == 0,
        TimeUnit::Minute => p.minute % step == 0,
        TimeUnit::Hour => p.hour % step == 0,
        TimeUnit::Day => (p.day - 1) % step == 0,
        TimeUnit::Week => true,
        TimeUnit::Month => (p.month - 1) % step == 0,
        TimeUnit::Year => p.year.rem_euclid(step) == 0,
    }
}

/// d3 time-scale ticks in [start, stop].
pub fn d3_time_ticks(start: f64, stop: f64, count: f64) -> Vec<f64> {
    let (a, b) = if start <= stop { (start, stop) } else { (stop, start) };
    if !(a.is_finite() && b.is_finite()) {
        return Vec::new();
    }
    let (unit, step) = d3_time_interval(a, b, count);
    if unit == TimeUnit::Millisecond {
        return linear_ticks(a, b, step.max(1) as f64, 0., 5000);
    }
    let mut out = Vec::new();
    let mut t = time_floor(a, unit);
    if t < a {
        t = time_offset(t, unit, 1);
    }
    let mut guard = 0;
    while t <= b + 0.5 && guard < 100_000 {
        if time_every_ok(t, unit, step) {
            out.push(t);
            if out.len() > 2000 {
                break;
            }
        }
        t = time_offset(t, unit, 1);
        guard += 1;
    }
    out
}

/// Vega's default multi-format for a time tick.
pub fn vega_time_label(t: f64) -> String {
    let p = time::parts(t);
    let fmt = if p.ms != 0. {
        ".%L"
    } else if p.second != 0 {
        ":%S"
    } else if p.minute != 0 {
        "%I:%M"
    } else if p.hour != 0 {
        "%I %p"
    } else if p.day != 1 {
        if p.weekday != 0 { "%a %d" } else { "%b %d" }
    } else if p.month != 1 {
        "%B"
    } else {
        "%Y"
    };
    time::strftime(fmt, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plotly_linear() {
        // A 4-bar chart's y axis on a 220px-ish plot: range [0, 9.47], nt ~ 6.5
        let nt = plotly_nticks(220., true, None);
        let dtick = plotly_linear_dtick(9.47 / nt);
        assert_eq!(dtick, 2.);
        assert_eq!(linear_ticks(0., 9.47, dtick, 0., 100), vec![0., 2., 4., 6., 8.]);
        assert_eq!(plotly_linear_dtick(0.57), 1.);
        assert_eq!(plotly_linear_dtick(0.11), 0.2);
        let t = linear_ticks(-0.25, 0.75, 0.1, 0., 100);
        assert_eq!(t[0], -0.2);
        assert_eq!(t[5], 0.3);
    }

    #[test]
    fn plotly_dates() {
        assert_eq!(plotly_date_dtick(40. * DAY).0, DateDtick::Months(2));
        let (d, t0) = plotly_date_dtick(20. * DAY);
        assert_eq!(d, DateDtick::Months(1));
        let lo = time::ms_from_ymd(2018, 1, 15);
        let hi = time::ms_from_ymd(2018, 6, 2);
        let ticks = date_ticks(lo, hi, d, t0, 100);
        assert_eq!(ticks.len(), 5);
        assert_eq!(time::strftime("%Y-%m-%d", ticks[0]), "2018-02-01");
        assert_eq!(plotly_date_round(d, t0), DateRound::Month);
        let (d, t0) = plotly_date_dtick(5. * DAY);
        assert_eq!(d, DateDtick::Ms(7. * DAY));
        assert_eq!(time::parts(t0).weekday, 0);
        let (d, _) = plotly_date_dtick(400. * DAY);
        assert_eq!(d, DateDtick::Months(24));
        let (d, _) = plotly_date_dtick(5. * HOUR);
        assert_eq!(d, DateDtick::Ms(6. * HOUR));
    }

    #[test]
    fn d3_ticks_and_nice() {
        assert_eq!(d3_ticks(0., 1., 5.), vec![0., 0.2, 0.4, 0.6, 0.8, 1.]);
        assert_eq!(d3_ticks(0., 10., 10.), (0..=10).map(|v| v as f64).collect::<Vec<_>>());
        assert_eq!(d3_nice(61., 249., 10.), (60., 260.));
        assert_eq!(d3_nice(0., 249., 10.), (0., 260.));
        assert_eq!(d3_nice(0.3, 41.9, 10.), (0., 45.));
        assert_eq!(d3_precision_fixed(0.5), 1);
        assert_eq!(d3_precision_fixed(5.), 0);
        assert_eq!(d3_precision_fixed(0.05), 2);
    }

    #[test]
    fn d3_time() {
        let a = time::ms_from_ymd(2022, 1, 1);
        let b = time::ms_from_ymd(2022, 12, 31);
        let t = d3_time_ticks(a, b, 8.);
        // ~quarterly ticks over a year (d3 picks month.every(3) for this span)
        assert!(t.len() >= 4 && t.len() <= 13, "{}", t.len());
        assert_eq!(vega_time_label(a), "2022");
        assert_eq!(vega_time_label(time::ms_from_ymd(2022, 4, 1)), "April");
        assert_eq!(vega_time_label(time::ms_from_ymd(2022, 4, 3)), "Apr 03");
    }
}
