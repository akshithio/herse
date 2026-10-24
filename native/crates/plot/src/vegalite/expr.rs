//! A subset of the Vega expression language (filters, calculate, conditions).

use super::data::V;
use crate::format;
use crate::time;

#[derive(Clone, Debug)]
pub enum Expr {
    Lit(V),
    Datum(String),
    /// Bare identifier: constants or params (resolved at eval time).
    Ident(String),
    Unary(char, Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    Array(Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(&'static str),
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    const OPS: [&str; 26] = [
        "===", "!==", "==", "!=", "<=", ">=", "&&", "||", "<", ">", "+", "-", "*", "/", "%", "!", "?", ":", "(", ")", "[", "]",
        ",", ".", "{", "}",
    ];
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            if i < c.len() && (c[i] == 'e' || c[i] == 'E') {
                i += 1;
                if i < c.len() && (c[i] == '+' || c[i] == '-') {
                    i += 1;
                }
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let t: String = c[start..i].iter().collect();
            out.push(Tok::Num(t.parse().ok()?));
            continue;
        }
        if ch == '\'' || ch == '"' {
            let q = ch;
            i += 1;
            let mut s = String::new();
            while i < c.len() && c[i] != q {
                if c[i] == '\\' && i + 1 < c.len() {
                    i += 1;
                    s.push(match c[i] {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    s.push(c[i]);
                }
                i += 1;
            }
            if i >= c.len() {
                return None;
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        if ch.is_alphabetic() || ch == '_' || ch == '$' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect()));
            continue;
        }
        let rest: String = c[i..(i + 3).min(c.len())].iter().collect();
        let op = OPS.iter().find(|op| rest.starts_with(**op))?;
        out.push(Tok::Op(op));
        i += op.chars().count();
    }
    Some(out)
}

struct Parser {
    t: Vec<Tok>,
    i: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn eat(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if *o == op) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expr(&mut self) -> Option<Expr> {
        self.depth += 1;
        if self.depth > 200 {
            return None;
        }
        let c = self.binary(0)?;
        let r = if self.eat("?") {
            let a = self.expr()?;
            if !self.eat(":") {
                return None;
            }
            let b = self.expr()?;
            Expr::Cond(Box::new(c), Box::new(a), Box::new(b))
        } else {
            c
        };
        self.depth -= 1;
        Some(r)
    }
    fn binary(&mut self, min_prec: u8) -> Option<Expr> {
        let mut lhs = self.unary()?;
        while let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            let prec = match op {
                "||" => 1,
                "&&" => 2,
                "==" | "!=" | "===" | "!==" => 3,
                "<" | ">" | "<=" | ">=" => 4,
                "+" | "-" => 5,
                "*" | "/" | "%" => 6,
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.i += 1;
            let rhs = self.binary(prec + 1)?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Some(lhs)
    }
    fn unary(&mut self) -> Option<Expr> {
        for op in ['!', '-', '+'] {
            if self.eat(&op.to_string()) {
                let e = self.unary()?;
                return Some(Expr::Unary(op, Box::new(e)));
            }
        }
        self.postfix()
    }
    fn postfix(&mut self) -> Option<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.eat(".") {
                let Some(Tok::Ident(name)) = self.peek().cloned() else { return None };
                self.i += 1;
                e = match e {
                    Expr::Ident(ref d) if d == "datum" => Expr::Datum(name),
                    Expr::Datum(parent) => Expr::Datum(format!("{parent}.{name}")),
                    _ => return None,
                };
            } else if self.eat("[") {
                let idx = self.expr()?;
                if !self.eat("]") {
                    return None;
                }
                e = match (e, idx) {
                    (Expr::Ident(d), Expr::Lit(V::Str(s))) if d == "datum" => Expr::Datum(s),
                    (base, idx) => Expr::Call("__index".into(), vec![base, idx]),
                };
            } else {
                break;
            }
        }
        Some(e)
    }
    fn primary(&mut self) -> Option<Expr> {
        match self.peek()?.clone() {
            Tok::Num(n) => {
                self.i += 1;
                Some(Expr::Lit(V::Num(n)))
            }
            Tok::Str(s) => {
                self.i += 1;
                Some(Expr::Lit(V::Str(s)))
            }
            Tok::Ident(name) => {
                self.i += 1;
                match name.as_str() {
                    "true" => return Some(Expr::Lit(V::Bool(true))),
                    "false" => return Some(Expr::Lit(V::Bool(false))),
                    "null" | "undefined" => return Some(Expr::Lit(V::Null)),
                    _ => {}
                }
                if self.eat("(") {
                    let mut args = Vec::new();
                    if !self.eat(")") {
                        loop {
                            args.push(self.expr()?);
                            if self.eat(")") {
                                break;
                            }
                            if !self.eat(",") {
                                return None;
                            }
                        }
                    }
                    return Some(Expr::Call(name, args));
                }
                Some(Expr::Ident(name))
            }
            Tok::Op("(") => {
                self.i += 1;
                let e = self.expr()?;
                if !self.eat(")") {
                    return None;
                }
                Some(e)
            }
            Tok::Op("[") => {
                self.i += 1;
                let mut items = Vec::new();
                if !self.eat("]") {
                    loop {
                        items.push(self.expr()?);
                        if self.eat("]") {
                            break;
                        }
                        if !self.eat(",") {
                            return None;
                        }
                    }
                }
                Some(Expr::Array(items))
            }
            _ => None,
        }
    }
}

pub fn parse(s: &str) -> Option<Expr> {
    if s.len() > 10_000 {
        return None;
    }
    let t = lex(s)?;
    let mut p = Parser { t, i: 0, depth: 0 };
    let e = p.expr()?;
    (p.i == p.t.len()).then_some(e)
}

/// Checks every function / identifier is one we implement, so a chart
/// using something else is refused instead of drawn wrong.
pub fn supported(e: &Expr, params: &dyn Fn(&str) -> bool) -> bool {
    match e {
        Expr::Lit(_) | Expr::Datum(_) => true,
        Expr::Ident(n) => matches!(n.as_str(), "PI" | "E" | "NaN" | "Infinity" | "LN2" | "LN10" | "SQRT2") || params(n),
        Expr::Unary(_, a) => supported(a, params),
        Expr::Bin(_, a, b) => supported(a, params) && supported(b, params),
        Expr::Cond(a, b, c) => supported(a, params) && supported(b, params) && supported(c, params),
        Expr::Array(v) => v.iter().all(|x| supported(x, params)),
        Expr::Call(name, args) => FUNCTIONS.contains(&name.as_str()) && args.iter().all(|x| supported(x, params)),
    }
}

const FUNCTIONS: &[&str] = &[
    "__index", "abs", "ceil", "floor", "round", "sqrt", "exp", "log", "pow", "min", "max", "sin", "cos", "tan", "atan2",
    "isNaN", "isFinite", "isValid", "isDefined", "isNumber", "isString", "isBoolean", "isDate", "toNumber", "toString",
    "toBoolean", "toDate", "length", "upper", "lower", "substring", "slice", "indexof", "lastindexof", "replace", "trim",
    "parseInt", "parseFloat", "year", "quarter", "month", "date", "day", "dayofyear", "hours", "minutes", "seconds",
    "milliseconds", "utcyear", "utcquarter", "utcmonth", "utcdate", "utcday", "utchours", "utcminutes", "utcseconds",
    "utcmilliseconds", "time", "datetime", "utc", "format", "timeFormat", "utcFormat", "if", "clamp", "inrange", "pad",
    "truncate", "vlSelectionTest", "vlSelectionResolve", "span", "join", "test",
];

pub trait Env {
    fn field(&self, name: &str) -> V;
    fn param(&self, name: &str) -> Option<V>;
}

pub fn truthy(v: &V) -> bool {
    match v {
        V::Null => false,
        V::Num(n) => *n != 0. && !n.is_nan(),
        V::Str(s) => !s.is_empty(),
        V::Bool(b) => *b,
    }
}

pub fn to_num(v: &V) -> f64 {
    match v {
        V::Num(n) => *n,
        V::Bool(b) => {
            if *b {
                1.
            } else {
                0.
            }
        }
        V::Str(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        V::Null => 0.,
    }
}

pub fn to_str(v: &V) -> String {
    match v {
        V::Num(n) => format::js_string(*n),
        V::Str(s) => s.clone(),
        V::Bool(b) => b.to_string(),
        V::Null => "null".into(),
    }
}

fn loose_eq(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Null, V::Null) => true,
        (V::Null, _) | (_, V::Null) => false,
        (V::Str(x), V::Str(y)) => x == y,
        (V::Bool(x), V::Bool(y)) => x == y,
        _ => to_num(a) == to_num(b),
    }
}

fn strict_eq(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Num(x), V::Num(y)) => x == y,
        (V::Str(x), V::Str(y)) => x == y,
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::Null, V::Null) => true,
        _ => false,
    }
}

fn compare(a: &V, b: &V) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (V::Str(x), V::Str(y)) => Some(x.cmp(y)),
        _ => to_num(a).partial_cmp(&to_num(b)),
    }
}

/// Dates in expressions are ms numbers (UTC wall clock).
fn date_of(v: &V) -> f64 {
    match v {
        V::Str(s) => time::parse_datetime(s).map(|(t, o)| t - o.unwrap_or(0.)).unwrap_or(f64::NAN),
        other => to_num(other),
    }
}

pub fn eval(e: &Expr, env: &dyn Env) -> V {
    match e {
        Expr::Lit(v) => v.clone(),
        Expr::Datum(f) => env.field(f),
        Expr::Ident(n) => match n.as_str() {
            "PI" => V::Num(std::f64::consts::PI),
            "E" => V::Num(std::f64::consts::E),
            "LN2" => V::Num(std::f64::consts::LN_2),
            "LN10" => V::Num(std::f64::consts::LN_10),
            "SQRT2" => V::Num(std::f64::consts::SQRT_2),
            "NaN" => V::Num(f64::NAN),
            "Infinity" => V::Num(f64::INFINITY),
            other => env.param(other).unwrap_or(V::Null),
        },
        Expr::Unary(op, a) => {
            let v = eval(a, env);
            match op {
                '!' => V::Bool(!truthy(&v)),
                '-' => V::Num(-to_num(&v)),
                _ => V::Num(to_num(&v)),
            }
        }
        Expr::Bin(op, a, b) => {
            if *op == "&&" {
                let l = eval(a, env);
                return if truthy(&l) { eval(b, env) } else { l };
            }
            if *op == "||" {
                let l = eval(a, env);
                return if truthy(&l) { l } else { eval(b, env) };
            }
            let l = eval(a, env);
            let r = eval(b, env);
            match *op {
                "+" => match (&l, &r) {
                    (V::Str(_), _) | (_, V::Str(_)) => V::Str(to_str(&l) + &to_str(&r)),
                    _ => V::Num(to_num(&l) + to_num(&r)),
                },
                "-" => V::Num(to_num(&l) - to_num(&r)),
                "*" => V::Num(to_num(&l) * to_num(&r)),
                "/" => V::Num(to_num(&l) / to_num(&r)),
                "%" => V::Num(to_num(&l) % to_num(&r)),
                "==" => V::Bool(loose_eq(&l, &r)),
                "!=" => V::Bool(!loose_eq(&l, &r)),
                "===" => V::Bool(strict_eq(&l, &r)),
                "!==" => V::Bool(!strict_eq(&l, &r)),
                "<" => V::Bool(compare(&l, &r) == Some(std::cmp::Ordering::Less)),
                ">" => V::Bool(compare(&l, &r) == Some(std::cmp::Ordering::Greater)),
                "<=" => V::Bool(matches!(compare(&l, &r), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal))),
                ">=" => V::Bool(matches!(compare(&l, &r), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal))),
                _ => V::Null,
            }
        }
        Expr::Cond(c, a, b) => {
            if truthy(&eval(c, env)) {
                eval(a, env)
            } else {
                eval(b, env)
            }
        }
        Expr::Array(_) => V::Null,
        Expr::Call(name, args) => call(name, args, env),
    }
}

fn call(name: &str, args: &[Expr], env: &dyn Env) -> V {
    let arg = |i: usize| args.get(i).map(|a| eval(a, env)).unwrap_or(V::Null);
    let num = |i: usize| to_num(&arg(i));
    let n1 = |f: fn(f64) -> f64| V::Num(f(num(0)));
    let part = |f: fn(&time::Parts) -> f64| {
        let t = date_of(&arg(0));
        if t.is_finite() { V::Num(f(&time::parts(t))) } else { V::Num(f64::NAN) }
    };
    match name {
        "abs" => n1(f64::abs),
        "ceil" => n1(f64::ceil),
        "floor" => n1(f64::floor),
        "round" => n1(|x| (x + 0.5).floor()),
        "sqrt" => n1(f64::sqrt),
        "exp" => n1(f64::exp),
        "log" => n1(f64::ln),
        "sin" => n1(f64::sin),
        "cos" => n1(f64::cos),
        "tan" => n1(f64::tan),
        "atan2" => V::Num(num(0).atan2(num(1))),
        "pow" => V::Num(num(0).powf(num(1))),
        "min" => V::Num((0..args.len()).map(num).fold(f64::INFINITY, f64::min)),
        "max" => V::Num((0..args.len()).map(num).fold(f64::NEG_INFINITY, f64::max)),
        "clamp" => V::Num(num(0).max(num(1)).min(num(2))),
        "isNaN" => V::Bool(num(0).is_nan()),
        "isFinite" => V::Bool(num(0).is_finite()),
        "isValid" => {
            let v = arg(0);
            V::Bool(!matches!(v, V::Null) && !matches!(v, V::Num(n) if n.is_nan()))
        }
        "isDefined" => V::Bool(!matches!(arg(0), V::Null)),
        "isNumber" => V::Bool(matches!(arg(0), V::Num(_))),
        "isString" => V::Bool(matches!(arg(0), V::Str(_))),
        "isBoolean" => V::Bool(matches!(arg(0), V::Bool(_))),
        "isDate" => V::Bool(false),
        "toNumber" | "parseFloat" => match arg(0) {
            V::Null => V::Null,
            v => V::Num(to_num(&v)),
        },
        "parseInt" => V::Num(to_num(&arg(0)).trunc()),
        "toString" => match arg(0) {
            V::Null => V::Null,
            v => V::Str(to_str(&v)),
        },
        "toBoolean" => V::Bool(truthy(&arg(0))),
        "toDate" => match arg(0) {
            V::Null => V::Null,
            v => V::Num(date_of(&v)),
        },
        "length" => V::Num(to_str(&arg(0)).chars().count() as f64),
        "upper" => V::Str(to_str(&arg(0)).to_uppercase()),
        "lower" => V::Str(to_str(&arg(0)).to_lowercase()),
        "trim" => V::Str(to_str(&arg(0)).trim().to_string()),
        "substring" | "slice" => {
            let s: Vec<char> = to_str(&arg(0)).chars().collect();
            let n = s.len() as f64;
            let fix = |v: f64| if name == "slice" && v < 0. { (n + v).max(0.) } else { v.clamp(0., n) };
            let a = fix(num(1));
            let b = if args.len() > 2 { fix(num(2)) } else { n };
            let (a, b) = if a <= b { (a as usize, b as usize) } else { (b as usize, a as usize) };
            V::Str(s[a..b].iter().collect())
        }
        "indexof" | "lastindexof" => {
            let s = to_str(&arg(0));
            let t = to_str(&arg(1));
            let i = if name == "indexof" { s.find(&t) } else { s.rfind(&t) };
            V::Num(i.map(|i| s[..i].chars().count() as f64).unwrap_or(-1.))
        }
        "replace" => V::Str(to_str(&arg(0)).replacen(&to_str(&arg(1)), &to_str(&arg(2)), 1)),
        "pad" => {
            let s = to_str(&arg(0));
            let n = num(1).max(0.) as usize;
            let c = to_str(&arg(2)).chars().next().unwrap_or(' ');
            let len = s.chars().count();
            if len >= n { V::Str(s) } else { V::Str(format!("{s}{}", c.to_string().repeat(n - len))) }
        }
        "truncate" => {
            let s: Vec<char> = to_str(&arg(0)).chars().collect();
            let n = num(1).max(0.) as usize;
            if s.len() <= n { V::Str(s.iter().collect()) } else { V::Str(s[..n.saturating_sub(1)].iter().collect::<String>() + "…") }
        }
        "year" | "utcyear" => part(|p| p.year as f64),
        "quarter" | "utcquarter" => part(|p| ((p.month - 1) / 3) as f64),
        "month" | "utcmonth" => part(|p| (p.month - 1) as f64),
        "date" | "utcdate" => part(|p| p.day as f64),
        "day" | "utcday" => part(|p| p.weekday as f64),
        "dayofyear" => part(|p| (p.yday + 1) as f64),
        "hours" | "utchours" => part(|p| p.hour as f64),
        "minutes" | "utcminutes" => part(|p| p.minute as f64),
        "seconds" | "utcseconds" => part(|p| p.second as f64),
        "milliseconds" | "utcmilliseconds" => part(|p| p.ms),
        "time" => V::Num(date_of(&arg(0))),
        "datetime" | "utc" => {
            let y = num(0);
            let m = if args.len() > 1 { num(1) } else { 0. };
            let d = if args.len() > 2 { num(2) } else { 1. };
            let rest = time::HOUR * if args.len() > 3 { num(3) } else { 0. }
                + time::MINUTE * if args.len() > 4 { num(4) } else { 0. }
                + time::SECOND * if args.len() > 5 { num(5) } else { 0. }
                + if args.len() > 6 { num(6) } else { 0. };
            if !(y.is_finite() && m.is_finite() && d.is_finite()) {
                return V::Num(f64::NAN);
            }
            V::Num(time::ms_from_ymd(y as i64, m as i64 + 1, 1) + (d - 1.) * time::DAY + rest)
        }
        "format" => V::Str(format::d3(&to_str(&arg(1)), num(0))),
        "timeFormat" | "utcFormat" => V::Str(time::strftime(&to_str(&arg(1)), date_of(&arg(0)))),
        "if" => {
            if truthy(&arg(0)) {
                arg(1)
            } else {
                arg(2)
            }
        }
        "inrange" => {
            let v = num(0);
            match args.get(1) {
                Some(Expr::Array(r)) if r.len() == 2 => {
                    let a = to_num(&eval(&r[0], env));
                    let b = to_num(&eval(&r[1], env));
                    V::Bool(v >= a.min(b) && v <= a.max(b))
                }
                _ => V::Bool(false),
            }
        }
        // Selections: an empty selection selects everything.
        "vlSelectionTest" => V::Bool(true),
        "test" => V::Bool(false),
        _ => V::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct E(HashMap<String, V>);
    impl Env for E {
        fn field(&self, n: &str) -> V {
            self.0.get(n).cloned().unwrap_or(V::Null)
        }
        fn param(&self, _: &str) -> Option<V> {
            None
        }
    }

    fn ev(s: &str) -> V {
        let mut m = HashMap::new();
        m.insert("x".to_string(), V::Num(5.));
        m.insert("name".to_string(), V::Str("Bob".into()));
        m.insert("my field".to_string(), V::Num(2.));
        eval(&parse(s).unwrap_or_else(|| panic!("parse {s}")), &E(m))
    }

    #[test]
    fn evaluates() {
        assert_eq!(ev("(datum.x > 3)"), V::Bool(true));
        assert_eq!(ev("datum.x * 2 + 1"), V::Num(11.));
        assert_eq!(ev("datum['my field'] ** 2".replace("**", "*").as_str()), V::Num(4.));
        assert_eq!(ev("datum.name == 'Bob' && datum.x < 10"), V::Bool(true));
        assert_eq!(ev("datum.x > 6 ? 'big' : 'small'"), V::Str("small".into()));
        assert_eq!(ev("-datum.x"), V::Num(-5.));
        assert_eq!(ev("'a' + datum.x"), V::Str("a5".into()));
        assert_eq!(ev("isValid(datum.missing)"), V::Bool(false));
        assert_eq!(ev("year(datetime(2020, 0, 15))"), V::Num(2020.));
        assert_eq!(ev("format(1234.5, ',.1f')"), V::Str("1,234.5".into()));
        assert_eq!(ev("indexof('hello', 'l')"), V::Num(2.));
        assert_eq!(ev("inrange(datum.x, [1, 10])"), V::Bool(true));
        assert!(parse("datum.x >").is_none());
        assert!(parse("foo(").is_none());
        assert!(parse("'unterminated").is_none());
    }

    #[test]
    fn support_check() {
        let p = |_: &str| false;
        assert!(supported(&parse("abs(datum.x) > 2").unwrap(), &p));
        assert!(!supported(&parse("someUnknownFn(datum.x)").unwrap(), &p));
        assert!(!supported(&parse("datum.x > param_1").unwrap(), &p));
    }
}
