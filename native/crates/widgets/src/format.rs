//! The subset of d3-format that ipywidgets' `readout_format` uses:
//! `[sign][0][width][,][.precision][~][type]` with types `d f e g r s %`
//! (and none). Fill/align and currency symbols are accepted and ignored.

pub fn format(spec: &str, value: f64) -> String {
    let spec = Spec::parse(spec);
    if !value.is_finite() {
        return if value.is_nan() { "NaN".into() } else if value > 0. { "Infinity".into() } else { "-Infinity".into() };
    }
    let negative = value < 0. || (value == 0. && value.is_sign_negative() && spec.ty != 'd');
    let v = value.abs();
    let (mut body, suffix) = match spec.ty {
        'd' => (format!("{}", v.round() as u64), String::new()),
        'f' => (fixed(v, spec.precision.unwrap_or(6)), String::new()),
        '%' => (fixed(v * 100., spec.precision.unwrap_or(6)), "%".into()),
        'e' => (to_exponential(v, spec.precision.unwrap_or(6)), String::new()),
        'g' => (to_precision(v, spec.precision.unwrap_or(6).max(1)), String::new()),
        'r' => (rounded(v, spec.precision.unwrap_or(6).max(1)), String::new()),
        's' => si(v, spec.precision.unwrap_or(6).max(1)),
        _ => {
            // No type: like `g` with insignificant zeros trimmed.
            let s = match spec.precision {
                Some(p) => to_precision(v, p.max(1)),
                None => shortest(v),
            };
            (trim_zeros(&s), String::new())
        }
    };
    if spec.trim {
        body = trim_zeros(&body);
    }
    if spec.comma {
        body = group(&body);
    }
    let negative = negative && body.bytes().any(|b| b.is_ascii_digit() && b != b'0');
    let sign = match (negative, spec.sign) {
        (true, '(') => "(",
        (true, _) => "−",
        (false, '+') => "+",
        (false, ' ') => " ",
        _ => "",
    };
    let close = if negative && spec.sign == '(' { ")" } else { "" };
    let mut out = format!("{sign}{body}{suffix}{close}");
    if spec.zero && out.chars().count() < spec.width {
        let pad = spec.width - out.chars().count();
        out.insert_str(sign.len(), &"0".repeat(pad));
    } else if out.chars().count() < spec.width {
        out = format!("{}{out}", " ".repeat(spec.width - out.chars().count()));
    }
    out
}

struct Spec {
    sign: char,
    zero: bool,
    width: usize,
    comma: bool,
    precision: Option<usize>,
    trim: bool,
    ty: char,
}

impl Spec {
    fn parse(spec: &str) -> Spec {
        let mut s = Spec { sign: '-', zero: false, width: 0, comma: false, precision: None, trim: false, ty: '\0' };
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        // [[fill]align]
        if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
            i = 2;
        } else if chars.first().is_some_and(|c| matches!(c, '<' | '>' | '^' | '=')) {
            i = 1;
        }
        if let Some(&c) = chars.get(i)
            && matches!(c, '-' | '+' | '(' | ' ')
        {
            s.sign = c;
            i += 1;
        }
        if chars.get(i).is_some_and(|c| matches!(c, '$' | '#')) {
            i += 1;
        }
        if chars.get(i) == Some(&'0') {
            s.zero = true;
            i += 1;
        }
        while let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
            s.width = s.width * 10 + d as usize;
            i += 1;
        }
        if chars.get(i) == Some(&',') {
            s.comma = true;
            i += 1;
        }
        if chars.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0;
            while let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
                p = p * 10 + d as usize;
                i += 1;
            }
            s.precision = Some(p.min(20));
        }
        if chars.get(i) == Some(&'~') {
            s.trim = true;
            i += 1;
        }
        if let Some(&c) = chars.get(i) {
            s.ty = match c {
                'd' | 'f' | 'e' | 'g' | 'r' | 's' | '%' => c,
                'F' => 'f',
                'E' => 'e',
                'G' => 'g',
                'i' => 'd',
                _ => '\0',
            };
            i += 1;
        }
        // d3 rejects a spec it can't read whole; draw the plain number instead.
        if i < chars.len() {
            return Spec { sign: '-', zero: false, width: 0, comma: false, precision: None, trim: false, ty: '\0' };
        }
        s
    }
}

/// JavaScript's `toFixed(p)`. Rust rounds an exact tie to even; JavaScript
/// rounds it up (`(0.25).toFixed(1)` is `"0.3"`).
fn fixed(v: f64, p: usize) -> String {
    format!("{:.*}", p, past_tie(v, &format!("{:.*}", p + 24, v), p))
}

/// `v`, nudged up by one ulp when its decimal expansion is exactly halfway
/// at `p` digits after the point of `digits` (`v` printed with extra digits).
fn past_tie(v: f64, digits: &str, p: usize) -> f64 {
    let frac = digits.split(['.', 'e']).nth(1).unwrap_or("");
    let tail = frac.get(p..).unwrap_or("");
    if tail.starts_with('5') && tail[1..].bytes().all(|b| b == b'0') && v > 0. {
        f64::from_bits(v.to_bits() + 1)
    } else {
        v
    }
}

/// JavaScript's `Number.prototype.toExponential(p)`: `1.00e+3`.
fn to_exponential(v: f64, p: usize) -> String {
    let v = past_tie(v, &format!("{:.*e}", p + 24, v), p);
    let s = format!("{:.*e}", p, v);
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    format!("{mantissa}e{}{}", if exp < 0 { '-' } else { '+' }, exp.abs())
}

/// JavaScript's `toPrecision(p)`: fixed unless the exponent is < -6 or >= p.
fn to_precision(v: f64, p: usize) -> String {
    if v == 0. {
        return format!("{:.*}", p - 1, 0.);
    }
    let e = exponent_after_rounding(v, p);
    if e < -6 || e >= p as i32 {
        to_exponential(v, p - 1)
    } else {
        fixed(v, (p as i32 - 1 - e).max(0) as usize)
    }
}

/// d3's `r`: rounded to `p` significant digits, always in decimal notation.
fn rounded(v: f64, p: usize) -> String {
    if v == 0. {
        return format!("{:.*}", p - 1, 0.);
    }
    let e = exponent_after_rounding(v, p);
    if e >= p as i32 - 1 {
        let scale = 10f64.powi(e - (p as i32 - 1));
        format!("{:.0}", (v / scale).round() * scale)
    } else {
        format!("{:.*}", (p as i32 - 1 - e) as usize, v)
    }
}

/// d3's `s`: significant digits with an SI prefix (`1.50k`, `2.00µ`).
fn si(v: f64, p: usize) -> (String, String) {
    const PREFIXES: [&str; 17] = ["y", "z", "a", "f", "p", "n", "µ", "m", "", "k", "M", "G", "T", "P", "E", "Z", "Y"];
    if v == 0. {
        return (format!("{:.*}", p - 1, 0.), String::new());
    }
    let e = exponent_after_rounding(v, p);
    let k = (e.div_euclid(3)).clamp(-8, 8);
    let scaled = v / 10f64.powi(k * 3);
    let digits = (p as i32 - 1 - (e - k * 3)).max(0) as usize;
    (format!("{:.*}", digits, scaled), PREFIXES[(k + 8) as usize].into())
}

fn exponent_after_rounding(v: f64, p: usize) -> i32 {
    let s = format!("{:.*e}", p.saturating_sub(1), v);
    s.split_once('e').and_then(|(_, e)| e.parse().ok()).unwrap_or(0)
}

/// Shortest round-trip decimal, the way JavaScript prints a number.
fn shortest(v: f64) -> String {
    if v != 0. && !(1e-6..1e21).contains(&v) {
        let s = format!("{v:e}");
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        return format!("{m}e{}{}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    format!("{v}")
}

fn trim_zeros(s: &str) -> String {
    let (mantissa, exp) = match s.find('e') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    if !mantissa.contains('.') {
        return s.to_string();
    }
    let t = mantissa.trim_end_matches('0').trim_end_matches('.');
    format!("{t}{exp}")
}

fn group(s: &str) -> String {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (int, rest) = s.split_at(end);
    let mut out = String::with_capacity(s.len() + int.len() / 3);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn d3_examples() {
        assert_eq!(format("d", 7.), "7");
        assert_eq!(format("d", 7.6), "8");
        assert_eq!(format(".2f", 0.25), "0.25");
        assert_eq!(format(".1f", 0.25), "0.3"); // JavaScript rounds exact ties up
        assert_eq!(format(".2f", 0.125), "0.13");
        assert_eq!(format(".1f", 0.15), "0.1"); // 0.15 is really 0.1499999…
        assert_eq!(format(".3g", 1000.), "1.00e+3");
        assert_eq!(format(".3g", 12.345), "12.3");
        assert_eq!(format(".3g", 0.001234), "0.00123");
        assert_eq!(format(".2e", 1234.), "1.23e+3");
        assert_eq!(format(".0%", 0.123), "12%");
        assert_eq!(format(".1%", 0.5), "50.0%");
        assert_eq!(format(",d", 1234567.), "1,234,567");
        assert_eq!(format(",.2f", 1234.5), "1,234.50");
        assert_eq!(format(".3s", 1500.), "1.50k");
        assert_eq!(format(".3~s", 1500.), "1.5k");
        assert_eq!(format(".2r", 1234.), "1200");
        assert_eq!(format(".3r", 0.0012345), "0.00123");
        assert_eq!(format("+d", 3.), "+3");
        assert_eq!(format("d", -3.), "−3");
        assert_eq!(format("05d", 42.), "00042");
        assert_eq!(format("", 0.1), "0.1");
        assert_eq!(format(".2~f", 1.50), "1.5");
        assert_eq!(format("d", -0.2), "0");
        assert_eq!(format("garbage", 2.5), "2.5");
    }
}
