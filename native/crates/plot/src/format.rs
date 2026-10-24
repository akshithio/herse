//! Number formatting: JS `Number` conversions, d3-format, plotly tick labels.

pub const MINUS: char = '\u{2212}';

/// Decimal digits and exponent of `x` rounded to `sig` significant digits
/// (`x.toExponential(sig - 1)`): ("12345", 3) for 1234.5 with sig 5.
fn exp_parts(x: f64, sig: usize) -> (String, i32) {
    let sig = sig.clamp(1, 100);
    let s = format!("{:.*e}", sig - 1, x.abs());
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    (digits, exp.parse().unwrap_or(0))
}

/// `x.toFixed(p)` (JS rounds exact ties away from zero; Rust to even).
pub fn js_fixed(x: f64, p: usize) -> String {
    if !x.is_finite() {
        return js_string(x);
    }
    let p = p.min(100);
    let scaled = x.abs() * 10f64.powi(p as i32);
    if scaled < 4.5e15 && scaled.fract() == 0.5 {
        let up = (scaled.floor() + 1.) / 10f64.powi(p as i32);
        let s = format!("{:.*}", p, up);
        return if x < 0. { format!("-{s}") } else { s };
    }
    let s = format!("{:.*}", p, x);
    // "-0.00" -> JS keeps the sign too ((-0.001).toFixed(2) === "-0.00").
    s
}

/// `x.toExponential(p)`.
pub fn js_exponential(x: f64, p: usize) -> String {
    if !x.is_finite() {
        return js_string(x);
    }
    let (digits, e) = exp_parts(x, p + 1);
    let mut s = String::new();
    if x < 0. {
        s.push('-');
    }
    s.push_str(&digits[..1]);
    if digits.len() > 1 {
        s.push('.');
        s.push_str(&digits[1..]);
    }
    s.push('e');
    s.push(if e < 0 { '-' } else { '+' });
    s.push_str(&e.abs().to_string());
    s
}

/// `x.toPrecision(p)`.
pub fn js_precision(x: f64, p: usize) -> String {
    if !x.is_finite() {
        return js_string(x);
    }
    let p = p.clamp(1, 100);
    if x == 0. {
        return if p > 1 { format!("0.{}", "0".repeat(p - 1)) } else { "0".into() };
    }
    let (digits, e) = exp_parts(x, p);
    if e < -6 || e >= p as i32 {
        return js_exponential(x, p - 1);
    }
    let mut s = String::new();
    if x < 0. {
        s.push('-');
    }
    if e < 0 {
        s.push_str("0.");
        for _ in 0..(-e - 1) {
            s.push('0');
        }
        s.push_str(&digits);
    } else {
        let int = (e + 1) as usize;
        s.push_str(&digits[..int.min(digits.len())]);
        if digits.len() > int {
            s.push('.');
            s.push_str(&digits[int..]);
        }
    }
    s
}

/// `String(x)` for a JS number.
pub fn js_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0. { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == 0. {
        return "0".into();
    }
    let a = x.abs();
    if (1e-6..1e21).contains(&a) {
        let s = format!("{x}");
        return s;
    }
    // Shortest round-trip digits in exponent form.
    let s = format!("{x:e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let e: i32 = exp.parse().unwrap_or(0);
    format!("{mant}e{}{}", if e < 0 { '-' } else { '+' }, e.abs())
}

/// Inserts thousands separators into the integer part of `s` (ASCII digits).
fn group(int: &str, sep: char) -> String {
    let n = int.len();
    let mut out = String::with_capacity(n + n / 3);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

/// d3-format's `formatTrim`: drops insignificant trailing zeros.
fn trim_zeros(s: &str) -> String {
    let b = s.as_bytes();
    let mut i0: isize = -1;
    let mut i1: isize = -1;
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'.' => {
                i0 = i as isize;
                i1 = i as isize;
            }
            b'0' => {
                if i0 == 0 {
                    i0 = i as isize;
                }
                i1 = i as isize;
            }
            c => {
                if !c.is_ascii_digit() || c == b'0' {
                    break;
                }
                if i0 > 0 {
                    i0 = 0;
                }
            }
        }
        i += 1;
    }
    if i0 > 0 {
        format!("{}{}", &s[..i0 as usize], &s[(i1 + 1) as usize..])
    } else {
        s.to_string()
    }
}

const SI: [&str; 17] = ["y", "z", "a", "f", "p", "n", "µ", "m", "", "k", "M", "G", "T", "P", "E", "Z", "Y"];

/// A parsed d3-format specifier.
#[derive(Clone, Debug)]
pub struct D3Format {
    fill: char,
    align: char,
    sign: char,
    symbol: Option<char>,
    zero: bool,
    width: usize,
    comma: bool,
    precision: Option<usize>,
    trim: bool,
    ty: char,
    /// Vega/plotly accept `%`-less prefixes like "$" handled by `symbol`.
    pub minus: char,
}

impl D3Format {
    pub fn parse(spec: &str) -> Option<D3Format> {
        let c: Vec<char> = spec.chars().collect();
        if c.len() > 40 {
            return None;
        }
        let mut i = 0;
        let mut fill = ' ';
        let mut align = '>';
        let is_align = |ch: char| matches!(ch, '<' | '>' | '=' | '^');
        if c.len() >= 2 && is_align(c[1]) {
            fill = c[0];
            align = c[1];
            i = 2;
        } else if !c.is_empty() && is_align(c[0]) {
            align = c[0];
            i = 1;
        }
        let mut sign = '-';
        if i < c.len() && matches!(c[i], '+' | '-' | '(' | ' ') {
            sign = c[i];
            i += 1;
        }
        let mut symbol = None;
        if i < c.len() && matches!(c[i], '$' | '#') {
            symbol = Some(c[i]);
            i += 1;
        }
        let mut zero = false;
        if i < c.len() && c[i] == '0' {
            zero = true;
            i += 1;
        }
        let mut width = 0usize;
        while i < c.len() && c[i].is_ascii_digit() {
            width = (width * 10 + c[i].to_digit(10)? as usize).min(200);
            i += 1;
        }
        let mut comma = false;
        if i < c.len() && c[i] == ',' {
            comma = true;
            i += 1;
        }
        let mut precision = None;
        if i < c.len() && c[i] == '.' {
            i += 1;
            let mut p = 0usize;
            let start = i;
            while i < c.len() && c[i].is_ascii_digit() {
                p = (p * 10 + c[i].to_digit(10)? as usize).min(100);
                i += 1;
            }
            if i == start {
                return None;
            }
            precision = Some(p);
        }
        let mut trim = false;
        if i < c.len() && c[i] == '~' {
            trim = true;
            i += 1;
        }
        let mut ty = ' ';
        if i < c.len() && (c[i].is_ascii_alphabetic() || c[i] == '%') {
            ty = c[i];
            i += 1;
        }
        if i != c.len() {
            return None;
        }
        if ty == 'n' {
            comma = true;
            ty = 'g';
        } else if !matches!(ty, '%' | 'b' | 'c' | 'd' | 'e' | 'f' | 'g' | 'o' | 'p' | 'r' | 's' | 'X' | 'x') {
            if precision.is_none() {
                precision = Some(12);
            }
            trim = true;
            ty = 'g';
        }
        if zero || (fill == '0' && align == '=') {
            zero = true;
            fill = '0';
            align = '=';
        }
        let precision = Some(match precision {
            None => 6,
            Some(p) if matches!(ty, 'g' | 'p' | 'r' | 's') => p.clamp(1, 21),
            Some(p) => p.min(20),
        });
        Some(D3Format {
            fill,
            align,
            sign,
            symbol,
            zero,
            width,
            comma,
            precision,
            trim,
            ty,
            minus: MINUS,
        })
    }

    pub fn format(&self, value: f64) -> String {
        let p = self.precision.unwrap_or(6);
        let mut prefix = String::new();
        let mut suffix = String::new();
        if self.symbol == Some('$') {
            prefix.push('$');
        } else if self.symbol == Some('#') && matches!(self.ty, 'b' | 'o' | 'x' | 'X') {
            prefix.push('0');
            prefix.push(self.ty.to_ascii_lowercase());
        }
        if matches!(self.ty, '%' | 'p') {
            suffix.push('%');
        }
        if self.ty == 'c' {
            return format!("{prefix}{}{suffix}", js_string(value));
        }
        let negative = value < 0. || (value == 0. && value.is_sign_negative());
        let a = value.abs();
        let mut si = 8i32;
        let mut body = if value.is_nan() {
            "NaN".to_string()
        } else {
            match self.ty {
                '%' => js_fixed(a * 100., p),
                'b' => format!("{:b}", a.round() as u64),
                'o' => format!("{:o}", a.round() as u64),
                'x' => format!("{:x}", a.round() as u64),
                'X' => format!("{:X}", a.round() as u64),
                'd' => {
                    if a.round() >= 1e21 {
                        format!("{:.0}", a.round())
                    } else {
                        js_string(a.round())
                    }
                }
                'e' => js_exponential(a, p),
                'f' => js_fixed(a, p),
                'g' => js_precision(a, p),
                'p' => rounded(a * 100., p),
                'r' => rounded(a, p),
                's' => {
                    let (s, e) = prefix_auto(a, p);
                    si = e;
                    s
                }
                _ => js_precision(a, p),
            }
        };
        if a.is_infinite() {
            body = "Infinity".into();
        }
        if self.trim {
            body = trim_zeros(&body);
        }
        let mut negative = negative;
        if negative && body.parse::<f64>().map(|v| v == 0.).unwrap_or(false) && self.sign != '+' {
            negative = false;
        }
        let sign_str = if negative {
            if self.sign == '(' { "(".to_string() } else { self.minus.to_string() }
        } else if self.sign == '-' || self.sign == '(' {
            String::new()
        } else {
            self.sign.to_string()
        };
        prefix = sign_str + &prefix;
        if self.ty == 's' {
            suffix = format!("{}{}", SI[(si + 8).clamp(0, 16) as usize], suffix);
        }
        if negative && self.sign == '(' {
            suffix.push(')');
        }
        // Split off the non-integer tail.
        if matches!(self.ty, 'd' | 'e' | 'f' | 'g' | 'p' | 'r' | 's' | '%')
            && let Some(pos) = body.find(|c: char| !c.is_ascii_digit()) {
                let tail = body[pos..].to_string();
                body.truncate(pos);
                suffix = tail + &suffix;
            }
        if self.comma && !self.zero {
            body = group(&body, ',');
        }
        let len = prefix.chars().count() + body.chars().count() + suffix.chars().count();
        let mut padding = if len < self.width {
            std::iter::repeat_n(self.fill, self.width - len).collect::<String>()
        } else {
            String::new()
        };
        if self.comma && self.zero {
            let digits = format!("{padding}{body}");
            body = group(&digits, ',');
            // Grouping lengthens; d3 trims back to the width.
            let over = (prefix.chars().count() + body.chars().count() + suffix.chars().count()).saturating_sub(self.width.max(len));
            if !padding.is_empty() && over > 0 {
                body = body.chars().skip(over).collect();
                if body.starts_with(',') {
                    body.remove(0);
                }
            }
            padding.clear();
        }
        match self.align {
            '<' => format!("{prefix}{body}{suffix}{padding}"),
            '=' => format!("{prefix}{padding}{body}{suffix}"),
            '^' => {
                let half = padding.chars().count() / 2;
                let left: String = padding.chars().take(half).collect();
                let right: String = padding.chars().skip(half).collect();
                format!("{left}{prefix}{body}{suffix}{right}")
            }
            _ => format!("{padding}{prefix}{body}{suffix}"),
        }
    }

}

/// d3 `formatRounded`.
fn rounded(x: f64, p: usize) -> String {
    if x == 0. {
        return "0".into();
    }
    let (coef, e) = exp_parts(x, p);
    if e < 0 {
        format!("0.{}{}", "0".repeat((-e - 1) as usize), coef)
    } else if coef.len() > (e + 1) as usize {
        format!("{}.{}", &coef[..(e + 1) as usize], &coef[(e + 1) as usize..])
    } else {
        format!("{}{}", coef, "0".repeat((e + 1) as usize - coef.len()))
    }
}

/// d3 `formatPrefixAuto`: returns the mantissa and the SI exponent / 3.
fn prefix_auto(x: f64, p: usize) -> (String, i32) {
    if x == 0. {
        let s = rounded(0., p);
        return (if p > 1 { format!("0.{}", "0".repeat(p - 1)) } else { s }, 0);
    }
    let (coef, e) = exp_parts(x, p);
    let k = (e as f64 / 3.).floor().clamp(-8., 8.) as i32;
    let i = e - k * 3 + 1;
    let n = coef.len() as i32;
    let s = if i == n {
        coef
    } else if i > n {
        format!("{}{}", coef, "0".repeat((i - n) as usize))
    } else if i > 0 {
        format!("{}.{}", &coef[..i as usize], &coef[i as usize..])
    } else {
        let (c2, _) = exp_parts(x, (p as i32 + i - 1).max(1) as usize);
        format!("0.{}{}", "0".repeat((-i) as usize), c2)
    };
    (s, k)
}

/// Formats with a d3 specifier, falling back to JS `String(x)`.
pub fn d3(spec: &str, x: f64) -> String {
    match D3Format::parse(spec) {
        Some(f) => f.format(x),
        None => js_string(x),
    }
}

// ---------------------------------------------------------------- plotly

/// plotly.js `Lib.numSeparate` with the default "." / "," separators.
pub fn num_separate(v: &str, separate_thousands: bool) -> String {
    let (int, frac) = match v.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (v, None),
    };
    let mut out = if frac.is_some() || int.len() > 4 || separate_thousands {
        group(int, ',')
    } else {
        int.to_string()
    };
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

const PLOTLY_SI: [&str; 10] = ["f", "p", "n", "μ", "m", "", "k", "M", "G", "T"];

/// Exponent formatting settings of a plotly axis.
#[derive(Clone, Debug)]
pub struct ExpFormat {
    /// "B" (default), "SI", "e", "E", "power", "none".
    pub format: String,
    pub separate_thousands: bool,
    pub minexponent: i32,
}

impl Default for ExpFormat {
    fn default() -> Self {
        ExpFormat {
            format: "B".into(),
            separate_thousands: false,
            minexponent: 3,
        }
    }
}

fn is_si(f: &str) -> bool {
    f == "SI" || f == "B"
}

fn beyond_si(e: i32) -> bool {
    !(-15..=14).contains(&e)
}

/// plotly.js `autoTickRound` for linear axes: (tickround, tickexponent).
pub fn plotly_tick_round(dtick: f64, range: (f64, f64), ef: &ExpFormat) -> (i32, i32) {
    let dtick = if dtick.is_finite() && dtick > 0. { dtick } else { 1. };
    let tickround = 2 - (dtick.log10() + 0.01).floor() as i32;
    let maxend = range.0.abs().max(range.1.abs());
    let mut exponent = 0;
    if maxend > 0. && maxend.is_finite() {
        let rangeexp = (maxend.log10() + 0.01).floor() as i32;
        if rangeexp.abs() > ef.minexponent {
            exponent = if is_si(&ef.format) && !beyond_si(rangeexp) {
                3 * ((rangeexp - 1) as f64 / 3.).round() as i32
            } else {
                rangeexp
            };
        }
    }
    (tickround, exponent)
}

/// plotly.js `numFormat` (tick labels without a `tickformat`).
pub fn plotly_num(v: f64, tickround: i32, exponent: i32, ef: &ExpFormat) -> String {
    if !v.is_finite() {
        return String::new();
    }
    let mut is_neg = v < 0.;
    let mut tickround = tickround.clamp(-30, 30);
    let mut exponent = exponent;
    let mut fmt = ef.format.as_str();
    if fmt == "none" {
        exponent = 0;
    }
    let e = 10f64.powi(-tickround) / 2.;
    let mut a = v.abs();
    let mut s;
    if a < e {
        // Zero never carries the exponent suffix (plotly's showexponent "all").
        s = "0".to_string();
        is_neg = false;
        exponent = 0;
    } else {
        a += e;
        if exponent != 0 {
            a *= 10f64.powi(-exponent);
            tickround += exponent;
        }
        if tickround == 0 {
            s = js_string(a.floor());
        } else if tickround < 0 {
            s = js_string(a.round());
            let keep = s.len() as i32 + tickround;
            if keep > 0 {
                s.truncate(keep as usize);
            } else {
                s.clear();
            }
            for _ in tickround..0 {
                s.push('0');
            }
        } else {
            s = js_string(a);
            if let Some(dp) = s.find('.') {
                let end = (dp + 1 + tickround as usize).min(s.len());
                s.truncate(end);
                while s.ends_with('0') {
                    s.pop();
                }
                if s.ends_with('.') {
                    s.pop();
                }
            }
            if s.contains('e') {
                // Tiny/huge values JS stringifies in exponent form.
                s = js_fixed(a, tickround.max(0) as usize);
                if s.contains('.') {
                    while s.ends_with('0') {
                        s.pop();
                    }
                    if s.ends_with('.') {
                        s.pop();
                    }
                }
            }
        }
        s = num_separate(&s, ef.separate_thousands);
    }
    if exponent != 0 && fmt != "hide" {
        if is_si(fmt) && beyond_si(exponent) {
            fmt = "power";
        }
        let signed = if exponent < 0 {
            format!("{MINUS}{}", -exponent)
        } else if fmt != "power" {
            format!("+{exponent}")
        } else {
            exponent.to_string()
        };
        match fmt {
            "e" | "E" => {
                s.push_str(fmt);
                s.push_str(&signed);
            }
            "power" => {
                s.push_str("×10");
                s.push_str(&superscript(&signed));
            }
            "B" if exponent == 9 => s.push('B'),
            f if is_si(f) => s.push_str(PLOTLY_SI[((exponent / 3) + 5).clamp(0, 9) as usize]),
            _ => {}
        }
    }
    if is_neg {
        format!("{MINUS}{s}")
    } else {
        s
    }
}

fn superscript(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' | MINUS => '⁻',
            c => c,
        })
        .collect()
}

/// plotly's hover-style formatting of a single value (what `%{y}` and
/// bar/heatmap texts show without an explicit format).
pub fn plotly_hover_num(v: f64, ef: &ExpFormat) -> String {
    if !v.is_finite() {
        return String::new();
    }
    let d = if v.abs() > 0. { v.abs() } else { 1. };
    let (tr, ex) = plotly_tick_round(d, (0., if v != 0. { v } else { 1. }), ef);
    plotly_num(v, tr + 4, ex, ef)
}

/// Replaces '-' with the true minus sign (plotly does this for d3 formats).
pub fn minus(s: String) -> String {
    if s.contains('-') { s.replace('-', "\u{2212}") } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_conversions() {
        assert_eq!(js_fixed(2.5, 0), "3");
        assert_eq!(js_fixed(1.005, 2), "1.00"); // binary 1.00499..
        assert_eq!(js_fixed(0.125, 2), "0.13");
        assert_eq!(js_precision(1234.5678, 3), "1.23e+3");
        assert_eq!(js_precision(0.000123, 2), "0.00012");
        assert_eq!(js_precision(45.6789, 3), "45.7");
        assert_eq!(js_precision(100., 3), "100");
        assert_eq!(js_exponential(12345., 2), "1.23e+4");
        assert_eq!(js_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(js_string(1e21), "1e+21");
        assert_eq!(js_string(1.5e-7), "1.5e-7");
        assert_eq!(js_string(-3.), "-3");
    }

    #[test]
    fn d3_formats() {
        assert_eq!(d3(",.2f", 1234.567), "1,234.57");
        assert_eq!(d3(".0%", 0.256), "26%");
        assert_eq!(d3(".1%", 0.256), "25.6%");
        assert_eq!(d3("$,.0f", 1234567.), "$1,234,567");
        assert_eq!(d3(".2s", 1234567.), "1.2M");
        assert_eq!(d3("~s", 1500.), "1.5k");
        assert_eq!(d3(".3~g", 0.5), "0.5");
        assert_eq!(d3("d", 42.4), "42");
        assert_eq!(d3(",", 1234567.), "1,234,567");
        assert_eq!(d3("", 0.1), "0.1");
        assert_eq!(d3(".1f", -0.04), "0.0");
        assert_eq!(d3(".1f", -1.5), "\u{2212}1.5");
        assert_eq!(d3("+.1f", 1.5), "+1.5");
        assert_eq!(d3("e", 1234.), "1.234000e+3");
        assert_eq!(d3("05d", 42.), "00042");
        assert_eq!(d3(">8.2f", 1.23456), "    1.23");
        assert_eq!(d3("x", 255.), "ff");
        assert_eq!(d3(".2r", 1234.), "1200");
        assert_eq!(d3(".3s", 0.000123), "123µ");
        assert!(D3Format::parse("not a format").is_none());
    }

    #[test]
    fn plotly_tick_labels() {
        let ef = ExpFormat::default();
        let lab = |v: f64, dtick: f64, r: (f64, f64)| {
            let (tr, ex) = plotly_tick_round(dtick, r, &ef);
            plotly_num(v, tr, ex, &ef)
        };
        assert_eq!(lab(2., 2., (0., 9.47)), "2");
        assert_eq!(lab(0.5, 0.5, (0., 3.)), "0.5");
        assert_eq!(lab(-1., 1., (-2., 2.)), "\u{2212}1");
        assert_eq!(lab(2000., 2000., (0., 9000.)), "2000");
        assert_eq!(lab(20000., 5000., (0., 25000.)), "20k");
        assert_eq!(lab(1.5e6, 5e5, (0., 5e6)), "1.5M");
        assert_eq!(lab(2e9, 1e9, (0., 5e9)), "2B");
        assert_eq!(lab(0., 1e9, (0., 5e9)), "0");
        assert_eq!(lab(0.0002, 0.0001, (0., 0.0005)), "200μ");
        assert_eq!(plotly_hover_num(1.25, &ef), "1.25");
        assert_eq!(plotly_hover_num(1.2e6, &ef), "1.2M");
        assert_eq!(plotly_hover_num(12.5, &ef), "12.5");
        assert_eq!(num_separate("1234.5", false), "1,234.5");
        assert_eq!(num_separate("1234", false), "1234");
        assert_eq!(num_separate("12345", false), "12,345");
    }
}
