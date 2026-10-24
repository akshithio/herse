//! Dates as milliseconds since 1970-01-01 (UTC wall clock), parsing and
//! d3-style strftime formatting.

pub const SECOND: f64 = 1000.;
pub const MINUTE: f64 = 60. * SECOND;
pub const HOUR: f64 = 60. * MINUTE;
pub const DAY: f64 = 24. * HOUR;
pub const WEEK: f64 = 7. * DAY;
/// plotly's average year / month (for picking tick units).
pub const AVG_YEAR: f64 = 365.25 * DAY;
pub const AVG_MONTH: f64 = AVG_YEAR / 12.;

/// Days since 1970-01-01 for a proleptic Gregorian date (month 1..=12).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // JavaScript dates span ±275,760 years; clamping a little past that keeps
    // the arithmetic below from overflowing on nonsense input.
    let (y, m, d) = (y.clamp(-300_000, 300_000), m.clamp(-1_000_000, 1_000_000), d.clamp(-1_000_000_000, 1_000_000_000));
    // Normalize month overflow (e.g. month 13) first.
    let y = y + (m - 1).div_euclid(12);
    let m = (m - 1).rem_euclid(12) + 1;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month 1..=12, day 1..=31) for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn ms_from_ymd(y: i64, m: i64, d: i64) -> f64 {
    days_from_civil(y, m, d) as f64 * DAY
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Parts {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    /// Fractional milliseconds within the second.
    pub ms: f64,
    /// 0 = Sunday.
    pub weekday: i64,
    /// 0-based day of year.
    pub yday: i64,
}

pub fn parts(t: f64) -> Parts {
    let t = if t.is_finite() { t.clamp(-8.64e15, 8.64e15) } else { 0. };
    let days = (t / DAY).floor();
    let rem = t - days * DAY;
    let days = days as i64;
    let (year, month, day) = civil_from_days(days);
    let secs = (rem / SECOND).floor();
    let ms = rem - secs * SECOND;
    let secs = secs as i64;
    Parts {
        year,
        month,
        day,
        hour: secs / 3600,
        minute: (secs / 60) % 60,
        second: secs % 60,
        ms,
        weekday: (days + 4).rem_euclid(7),
        yday: days - days_from_civil(year, 1, 1),
    }
}

/// Parses an ISO-ish date/time string: `YYYY`, `YYYY-MM`, `YYYY-MM-DD`,
/// optional `[ T]HH[:MM[:SS[.fff]]]` and an optional `Z` / `±HH[:MM]`.
/// Returns (ms of the wall-clock time, offset in ms if one was given).
pub fn parse_datetime(s: &str) -> Option<(f64, Option<f64>)> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 4 || b.len() > 40 {
        return None;
    }
    let mut i = 0;
    let neg = b[0] == b'-';
    if neg {
        i = 1;
    }
    let digits = |i: &mut usize, min: usize, max: usize| -> Option<i64> {
        let start = *i;
        while *i < b.len() && *i - start < max && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i - start < min {
            return None;
        }
        std::str::from_utf8(&b[start..*i]).ok()?.parse().ok()
    };
    let mut year = digits(&mut i, 4, 4)?;
    if neg {
        year = -year;
    }
    let (mut month, mut day) = (1, 1);
    let (mut h, mut mi, mut sec, mut frac) = (0, 0, 0, 0.);
    let mut offset = None;
    if i < b.len() && b[i] == b'-' {
        i += 1;
        month = digits(&mut i, 1, 2)?;
        if i < b.len() && b[i] == b'-' {
            i += 1;
            day = digits(&mut i, 1, 2)?;
            if i < b.len() && (b[i] == b'T' || b[i] == b't' || b[i] == b' ') {
                i += 1;
                h = digits(&mut i, 1, 2)?;
                if i < b.len() && b[i] == b':' {
                    i += 1;
                    mi = digits(&mut i, 2, 2)?;
                    if i < b.len() && b[i] == b':' {
                        i += 1;
                        sec = digits(&mut i, 2, 2)?;
                        if i < b.len() && (b[i] == b'.' || b[i] == b',') {
                            i += 1;
                            let start = i;
                            while i < b.len() && b[i].is_ascii_digit() {
                                i += 1;
                            }
                            if i == start {
                                return None;
                            }
                            let f = std::str::from_utf8(&b[start..i]).ok()?;
                            frac = format!("0.{f}").parse::<f64>().ok()?;
                        }
                    }
                }
                if i < b.len() {
                    match b[i] {
                        b'Z' | b'z' => {
                            offset = Some(0.);
                            i += 1;
                        }
                        b'+' | b'-' => {
                            let sign = if b[i] == b'-' { -1. } else { 1. };
                            i += 1;
                            let oh = digits(&mut i, 2, 2)?;
                            if i < b.len() && b[i] == b':' {
                                i += 1;
                            }
                            let om = if i < b.len() { digits(&mut i, 2, 2)? } else { 0 };
                            offset = Some(sign * (oh as f64 * HOUR + om as f64 * MINUTE));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    if i != b.len() {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || h > 24 || mi > 59 || sec > 60 {
        return None;
    }
    let t = ms_from_ymd(year, month, day)
        + h as f64 * HOUR
        + mi as f64 * MINUTE
        + sec as f64 * SECOND
        + frac * SECOND;
    Some((t, offset))
}

/// True for strings that look like dates (not plain numbers).
pub fn is_date_string(s: &str) -> bool {
    parse_datetime(s).is_some()
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
    "November", "December",
];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

fn pad(out: &mut String, v: i64, width: usize, padc: Option<char>) {
    let s = v.abs().to_string();
    if v < 0 {
        out.push('-');
    }
    if let Some(c) = padc {
        for _ in s.len()..width {
            out.push(c);
        }
    }
    out.push_str(&s);
}

/// d3-time-format style formatting (plus plotly's `%{n}f` and `%h`).
pub fn strftime(fmt: &str, t: f64) -> String {
    let p = parts(t);
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut padc: Option<Option<char>> = None;
        if let Some(&m) = chars.peek()
            && (m == '-' || m == '_' || m == '0')
        {
            chars.next();
            padc = Some(match m {
                '-' => None,
                '_' => Some(' '),
                _ => Some('0'),
            });
        }
        let Some(d) = chars.next() else {
            out.push('%');
            break;
        };
        let zp = |default: char| padc.unwrap_or(Some(default));
        match d {
            'a' => out.push_str(&DAYS[p.weekday as usize][..3]),
            'A' => out.push_str(DAYS[p.weekday as usize]),
            'b' => out.push_str(&MONTHS[(p.month - 1) as usize][..3]),
            'B' => out.push_str(MONTHS[(p.month - 1) as usize]),
            'c' => out.push_str(&strftime("%-m/%-d/%Y, %-I:%M:%S %p", t)),
            'd' => pad(&mut out, p.day, 2, zp('0')),
            'e' => pad(&mut out, p.day, 2, zp(' ')),
            'f' => pad(&mut out, (p.ms * 1000.).floor() as i64, 6, zp('0')),
            'H' => pad(&mut out, p.hour, 2, zp('0')),
            'I' => pad(&mut out, (p.hour + 11) % 12 + 1, 2, zp('0')),
            'j' => pad(&mut out, p.yday + 1, 3, zp('0')),
            'L' => pad(&mut out, p.ms.floor() as i64, 3, zp('0')),
            'm' => pad(&mut out, p.month, 2, zp('0')),
            'M' => pad(&mut out, p.minute, 2, zp('0')),
            'p' => out.push_str(if p.hour >= 12 { "PM" } else { "AM" }),
            'q' => pad(&mut out, (p.month - 1) / 3 + 1, 1, None),
            'h' => pad(&mut out, (p.month - 1) / 6 + 1, 1, None),
            'Q' => out.push_str(&format!("{}", t.round())),
            's' => out.push_str(&format!("{}", (t / 1000.).floor())),
            'S' => pad(&mut out, p.second, 2, zp('0')),
            'u' => pad(&mut out, if p.weekday == 0 { 7 } else { p.weekday }, 1, None),
            'w' => pad(&mut out, p.weekday, 1, None),
            'U' => pad(&mut out, (p.yday + 7 - p.weekday) / 7, 2, zp('0')),
            'W' => pad(&mut out, (p.yday + 7 - (p.weekday + 6) % 7) / 7, 2, zp('0')),
            'V' => pad(&mut out, iso_week(&p), 2, zp('0')),
            'x' => out.push_str(&strftime("%-m/%-d/%Y", t)),
            'X' => out.push_str(&strftime("%-I:%M:%S %p", t)),
            'y' => pad(&mut out, p.year.rem_euclid(100), 2, zp('0')),
            'Y' => pad(&mut out, p.year, 4, zp('0')),
            'G' => pad(&mut out, p.year, 4, zp('0')),
            'Z' => out.push_str("+0000"),
            '%' => out.push('%'),
            '{' => {
                // plotly: %{n}f = fractional seconds with n digits.
                let mut n = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    n.push(c);
                }
                let n: usize = n.parse().unwrap_or(3).min(9);
                if chars.peek() == Some(&'f') {
                    chars.next();
                }
                let frac = p.ms / 1000.;
                let s = format!("{:.*}", n, frac);
                out.push_str(s.get(2..).unwrap_or(""));
            }
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn iso_week(p: &Parts) -> i64 {
    let wd = (p.weekday + 6) % 7; // Monday = 0
    let w = (p.yday - wd + 10) / 7;
    if w < 1 {
        return 52;
    }
    if w > 52 {
        let days_in_year = if is_leap(p.year) { 366 } else { 365 };
        if p.yday - wd + 3 >= days_in_year {
            return 1;
        }
    }
    w
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Adds `months` calendar months, keeping the time of day (the day of month
/// is clamped to the target month's length).
pub fn add_months(t: f64, months: i64) -> f64 {
    let p = parts(t);
    let total = p.year * 12 + (p.month - 1) + months;
    let y = total.div_euclid(12);
    let m = total.rem_euclid(12) + 1;
    ms_from_ymd(y, m, p.day.min(days_in_month(y, m)))
        + (p.hour as f64 * HOUR + p.minute as f64 * MINUTE + p.second as f64 * SECOND + p.ms)
}

pub fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        for z in [-800000, -1, 0, 1, 11016, 19000, 2932896] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn parses_dates() {
        let (t, off) = parse_datetime("2020-03-04").unwrap();
        assert_eq!(off, None);
        let p = parts(t);
        assert_eq!((p.year, p.month, p.day), (2020, 3, 4));
        let (t, off) = parse_datetime("2020-03-04T05:06:07.25Z").unwrap();
        assert_eq!(off, Some(0.));
        let p = parts(t);
        assert_eq!((p.hour, p.minute, p.second, p.ms), (5, 6, 7, 250.));
        assert!(parse_datetime("2020-03-04 05:06:07+05:30").is_some());
        assert!(parse_datetime("2020").is_some());
        assert!(parse_datetime("2020-13-01").is_none());
        assert!(parse_datetime("hello").is_none());
        assert!(parse_datetime("12.5").is_none());
        assert!(parse_datetime("2020-01-01T").is_none());
    }

    #[test]
    fn formats() {
        let t = parse_datetime("2024-03-01T15:04:05").unwrap().0;
        assert_eq!(strftime("%Y-%m-%d %H:%M:%S", t), "2024-03-01 15:04:05");
        assert_eq!(strftime("%b %-d, %Y", t), "Mar 1, 2024");
        assert_eq!(strftime("%a %d %B", t), "Fri 01 March");
        assert_eq!(strftime("%I %p", t), "03 PM");
        assert_eq!(strftime("Q%q %y", t), "Q1 24");
        assert_eq!(strftime("%j", t), "061");
        assert_eq!(parts(t).weekday, 5);
        assert_eq!(strftime("%{2}f", t + 123.), "12");
    }

    #[test]
    fn month_arithmetic() {
        let t = ms_from_ymd(2023, 11, 1);
        let p = parts(add_months(t, 3));
        assert_eq!((p.year, p.month, p.day), (2024, 2, 1));
        let p = parts(add_months(t, -12));
        assert_eq!((p.year, p.month), (2022, 11));
    }
}
