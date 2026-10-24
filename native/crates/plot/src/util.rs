//! JSON access helpers, plotly typed arrays, small numeric utilities.

use std::collections::HashMap;

use serde_json::Value;

/// `a.b.c` lookup through nested objects.
pub fn path<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    let mut cur = v;
    for k in p.split('.') {
        cur = cur.as_object()?.get(k)?;
    }
    (!cur.is_null()).then_some(cur)
}

pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Numbers, or strings that parse as numbers (plotly's `cleanNumber`).
pub fn num_loose(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => parse_num(s),
        Value::Bool(b) => Some(if *b { 1. } else { 0. }),
        _ => None,
    }
}

pub fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim().trim_start_matches('$').replace(',', "");
    let t = t.trim_end_matches('%');
    if t.is_empty() || t.len() > 40 {
        return None;
    }
    let b = t.as_bytes();
    // Reject things Rust parses but JS's Number() / plotly wouldn't (inf, nan).
    if !b.iter().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E')) {
        return None;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// A JSON value rendered as a category label (like JS `String(v)`).
pub fn label_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map(crate::format::js_string).unwrap_or_default(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(a) => a.iter().map(label_of).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// A plotly data array: plain JSON, or a typed array
/// (`{"dtype": "f8", "bdata": "<base64>", "shape": "r, c"}`, plotly >= 6).
#[derive(Clone, Debug)]
pub enum Arr<'a> {
    Json(&'a [Value]),
    Num(std::rc::Rc<Vec<f64>>),
}

thread_local! {
    /// Decoded typed arrays for the render in progress (keyed by address:
    /// the same JSON value is read several times per render).
    static TYPED: std::cell::RefCell<HashMap<usize, std::rc::Rc<Vec<f64>>>> = std::cell::RefCell::new(HashMap::new());
}

/// Drops cached typed-array decodes (call around each render).
pub fn clear_typed_cache() {
    TYPED.with(|c| c.borrow_mut().clear());
}

impl<'a> Arr<'a> {
    pub fn from(v: &'a Value) -> Option<Arr<'a>> {
        match v {
            Value::Array(a) => Some(Arr::Json(a)),
            Value::Object(_) => {
                let key = v as *const Value as usize;
                if let Some(hit) = TYPED.with(|c| c.borrow().get(&key).cloned()) {
                    return Some(Arr::Num(hit));
                }
                let (d, _) = typed_array(v)?;
                let rc = std::rc::Rc::new(d);
                TYPED.with(|c| c.borrow_mut().insert(key, rc.clone()));
                Some(Arr::Num(rc))
            }
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Arr::Json(a) => a.len(),
            Arr::Num(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Datum<'_> {
        match self {
            Arr::Json(a) => match a.get(i) {
                Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).map(Datum::Num).unwrap_or(Datum::Null),
                Some(Value::String(s)) => Datum::Str(s),
                Some(Value::Bool(b)) => Datum::Bool(*b),
                Some(Value::Array(_)) | Some(Value::Object(_)) => Datum::Other,
                _ => Datum::Null,
            },
            Arr::Num(v) => v.get(i).copied().filter(|f| f.is_finite()).map(Datum::Num).unwrap_or(Datum::Null),
        }
    }

    pub fn num(&self, i: usize) -> Option<f64> {
        match self.get(i) {
            Datum::Num(f) => Some(f),
            Datum::Str(s) => parse_num(s),
            _ => None,
        }
    }

    pub fn nums(&self) -> Vec<Option<f64>> {
        (0..self.len()).map(|i| self.num(i)).collect()
    }

}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Datum<'a> {
    Num(f64),
    Str(&'a str),
    Bool(bool),
    Null,
    Other,
}

impl Datum<'_> {
    pub fn label(&self) -> String {
        match self {
            Datum::Num(f) => crate::format::js_string(*f),
            Datum::Str(s) => s.to_string(),
            Datum::Bool(b) => b.to_string(),
            Datum::Null | Datum::Other => String::new(),
        }
    }
}

/// A 2-D numeric matrix (heatmap z): rows of JSON arrays / typed arrays, or
/// one typed array with a shape.
pub fn matrix(v: &Value) -> Option<Vec<Vec<Option<f64>>>> {
    match v {
        Value::Array(rows) => {
            if rows.iter().all(|r| !r.is_array() && !r.is_object()) {
                // A 1-D z: one row.
                return Some(vec![rows.iter().map(num_loose).collect()]);
            }
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                match Arr::from(r) {
                    Some(a) => out.push(a.nums()),
                    None => out.push(Vec::new()),
                }
            }
            Some(out)
        }
        Value::Object(_) => {
            let (data, shape) = typed_array(v)?;
            match shape.as_slice() {
                [r, c] if *r * *c == data.len() && *c > 0 => {
                    Some(data.chunks(*c).map(|ch| ch.iter().map(|x| x.is_finite().then_some(*x)).collect()).collect())
                }
                _ => Some(vec![data.into_iter().map(|x| x.is_finite().then_some(x)).collect()]),
            }
        }
        _ => None,
    }
}

/// Decodes a plotly typed array spec into f64s (+ shape).
pub fn typed_array(v: &Value) -> Option<(Vec<f64>, Vec<usize>)> {
    let o = v.as_object()?;
    let dtype = o.get("dtype")?.as_str()?;
    let bdata = o.get("bdata")?.as_str()?;
    if bdata.len() > 512 * 1024 * 1024 {
        return None;
    }
    let bytes = base64(bdata)?;
    let (size, conv): (usize, fn(&[u8]) -> f64) = match dtype {
        "f8" | "float64" => (8, |b| f64::from_le_bytes(b.try_into().unwrap_or([0; 8]))),
        "f4" | "float32" => (4, |b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])) as f64),
        "i4" | "int32" => (4, |b| i32::from_le_bytes(b.try_into().unwrap_or([0; 4])) as f64),
        "u4" | "uint32" => (4, |b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])) as f64),
        "i2" | "int16" => (2, |b| i16::from_le_bytes(b.try_into().unwrap_or([0; 2])) as f64),
        "u2" | "uint16" => (2, |b| u16::from_le_bytes(b.try_into().unwrap_or([0; 2])) as f64),
        "i1" | "int8" => (1, |b| b[0] as i8 as f64),
        "u1" | "uint8" | "u1c" | "uint8clamped" => (1, |b| b[0] as f64),
        _ => return None,
    };
    let data: Vec<f64> = bytes.chunks_exact(size).map(conv).collect();
    let shape = match o.get("shape") {
        Some(Value::String(s)) => s.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_u64().map(|u| u as usize)).collect(),
        Some(Value::Number(n)) => n.as_u64().map(|u| vec![u as usize]).unwrap_or_default(),
        _ => vec![data.len()],
    };
    Some((data, shape))
}

/// Standard base64 (padding optional, whitespace ignored).
pub fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A tiny uncompressed (stored-deflate) RGBA PNG encoder for heatmap images.
pub fn png_rgba(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c: u32 = 0xffff_ffff;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = Vec::with_capacity(4 + data.len());
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for row in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgba[row * w * 4..(row + 1) * w * 4]);
    }
    let mut z = vec![0x78, 0x01];
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, blk) in blocks.iter().enumerate() {
        z.push(if i + 1 == blocks.len() { 1 } else { 0 });
        let len = blk.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(blk);
    }
    if blocks.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// `v.clamp(lo, hi)` that tolerates NaN / inverted bounds (std panics).
pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if lo <= hi { v.max(lo).min(hi) } else { v }
}

/// Finite min / max of an iterator.
pub fn extent(it: impl IntoIterator<Item = f64>) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for v in it {
        if v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    (lo <= hi).then_some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "hello world"] {
            assert_eq!(base64(&base64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert!(base64("!!").is_none());
    }

    #[test]
    fn typed_arrays() {
        let bytes: Vec<u8> = [1.5f64, -2., 3.25].iter().flat_map(|f| f.to_le_bytes()).collect();
        let v = serde_json::json!({"dtype": "f8", "bdata": base64_encode(&bytes)});
        let a = Arr::from(&v).unwrap();
        assert_eq!(a.nums(), vec![Some(1.5), Some(-2.), Some(3.25)]);
        let bytes: Vec<u8> = (0..6i16).flat_map(|i| i.to_le_bytes()).collect();
        let v = serde_json::json!({"dtype": "i2", "bdata": base64_encode(&bytes), "shape": "2, 3"});
        let m = matrix(&v).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[1], vec![Some(3.), Some(4.), Some(5.)]);
        assert!(Arr::from(&serde_json::json!({"dtype": "q9", "bdata": ""})).is_none());
    }


    #[test]
    fn png_is_well_formed() {
        let png = png_rgba(2, 2, &[255; 16]);
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn safe_clamp() {
        assert_eq!(clamp(5., 0., 1.), 1.);
        assert_eq!(clamp(5., f64::NAN, 1.), 5.);
        assert_eq!(clamp(5., 3., 1.), 5.);
    }
}
