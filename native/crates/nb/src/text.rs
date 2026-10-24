//! Text primitives that must match Python exactly: `str.splitlines(True)`,
//! `json.dumps` string escaping, and `float.__repr__`.

/// Iterator over Python `str.splitlines(keepends=True)` pieces.
///
/// Line boundaries are `\n`, `\r`, `\r\n`, `\v`, `\f`, `\x1c`, `\x1d`, `\x1e`,
/// U+0085, U+2028 and U+2029; each piece keeps its terminator, and there is
/// no trailing empty piece (`""` yields nothing, `"a\n"` yields `["a\n"]`).
pub struct SplitLines<'a> {
    rest: &'a str,
}

/// Python `s.splitlines(True)`.
pub fn split_lines(s: &str) -> SplitLines<'_> {
    SplitLines { rest: s }
}

impl<'a> Iterator for SplitLines<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        if self.rest.is_empty() {
            return None;
        }
        let end = line_end(self.rest.as_bytes());
        let (line, rest) = self.rest.split_at(end);
        self.rest = rest;
        Some(line)
    }
}

/// Bytes that can start a line boundary.
static BREAK_START: [bool; 256] = {
    let mut t = [false; 256];
    t[b'\n' as usize] = true;
    t[b'\r' as usize] = true;
    t[0x0b] = true;
    t[0x0c] = true;
    t[0x1c] = true;
    t[0x1d] = true;
    t[0x1e] = true;
    t[0xc2] = true; // U+0085 = C2 85
    t[0xe2] = true; // U+2028/U+2029 = E2 80 A8/A9
    t
};

/// Byte length of the first line of `b` including its terminator.
#[inline]
fn line_end(b: &[u8]) -> usize {
    first_line(b).0
}

/// `(end, terminated)`: byte length of the first Python line of `b`
/// (including its terminator) and whether it has a terminator.
#[inline]
pub(crate) fn first_line(b: &[u8]) -> (usize, bool) {
    let mut i = 0;
    loop {
        // Skip bytes that cannot start a break: 16 at a time with NEON
        // (break starts are < 0x20 or the UTF-8 leads 0xC2 / 0xE2), then 8.
        #[cfg(target_arch = "aarch64")]
        {
            use std::arch::aarch64::*;
            // SAFETY: NEON is baseline on aarch64; loads read b[i..i + 16] with i + 16 <= b.len().
            unsafe {
                let space = vdupq_n_u8(0x20);
                let lead = vdupq_n_u8(0xc2);
                while i + 16 <= b.len() {
                    let v = vld1q_u8(b.as_ptr().add(i));
                    let m = vorrq_u8(vcltq_u8(v, space), vcgeq_u8(v, lead));
                    let nib = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m))));
                    if nib != 0 {
                        i += (nib.trailing_zeros() / 4) as usize;
                        break;
                    }
                    i += 16;
                }
            }
        }
        while i + 8 <= b.len() {
            let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
            let lt = w.wrapping_sub(LO * 0x20) & !w & HI;
            if (lt | (w & HI)) != 0 {
                break;
            }
            i += 8;
        }
        if i >= b.len() {
            return (b.len(), false);
        }
        let c = b[i];
        if BREAK_START[c as usize] {
            match c {
                b'\r' => return (if b.get(i + 1) == Some(&b'\n') { i + 2 } else { i + 1 }, true),
                0xc2 => {
                    if b.get(i + 1) == Some(&0x85) {
                        return (i + 2, true);
                    }
                }
                0xe2 => {
                    if b.get(i + 1) == Some(&0x80) && matches!(b.get(i + 2), Some(0xa8 | 0xa9)) {
                        return (i + 3, true);
                    }
                }
                _ => return (i + 1, true),
            }
        }
        i += 1;
    }
}

/// Fast 64-bit content hash (not cryptographic), used to recognise fields
/// whose on-disk line chunking must be reproduced.
pub(crate) fn content_hash(s: &str) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let b = s.as_bytes();
    let mut h: u64 = (b.len() as u64).wrapping_mul(K);
    let (words, rest) = b.as_chunks::<8>();
    for w in words {
        h = (h.rotate_left(23) ^ u64::from_le_bytes(*w)).wrapping_mul(K);
    }
    let mut tail = [0u8; 8];
    tail[..rest.len()].copy_from_slice(rest);
    h = (h.rotate_left(23) ^ u64::from_le_bytes(tail)).wrapping_mul(K);
    h ^ (h >> 29)
}

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// Whether any byte of `w` needs escaping in a JSON string: `< 0x20`, `"`,
/// `\`, and (when `ascii`) `>= 0x7f` (Python's `ensure_ascii` escapes DEL too).
#[inline(always)]
fn word_needs_escape(w: u64, ascii: bool) -> bool {
    let lt = w.wrapping_sub(LO * 0x20) & !w & HI;
    let q = w ^ (LO * 0x22);
    let q = q.wrapping_sub(LO) & !q & HI;
    let bs = w ^ (LO * 0x5c);
    let bs = bs.wrapping_sub(LO) & !bs & HI;
    let hi = if ascii {
        let d = w ^ (LO * 0x7f);
        (w & HI) | (d.wrapping_sub(LO) & !d & HI)
    } else {
        0
    };
    (lt | q | bs | hi) != 0
}

static HEX: &[u8; 16] = b"0123456789abcdef";

/// Index of the first byte at or after `i` that needs escaping, or a position
/// with fewer than 16 bytes left (the caller finishes with the SWAR loop).
#[cfg(target_arch = "aarch64")]
#[inline]
fn skip_clean_simd(b: &[u8], mut i: usize, ascii: bool) -> usize {
    use std::arch::aarch64::*;
    // SAFETY: NEON is part of the aarch64 baseline; every load reads 16 bytes
    // at `i` / `i + 16` with `i + 32 <= b.len()`.
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let bslash = vdupq_n_u8(b'\\');
        let space = vdupq_n_u8(0x20);
        let del = vdupq_n_u8(if ascii { 0x7f } else { 0xff });
        let ff = vdupq_n_u8(if ascii { 0x00 } else { 0xff });
        while i + 32 <= b.len() {
            let a = vld1q_u8(b.as_ptr().add(i));
            let c = vld1q_u8(b.as_ptr().add(i + 16));
            let ma = vorrq_u8(vorrq_u8(vceqq_u8(a, quote), vceqq_u8(a, bslash)), vcltq_u8(a, space));
            let mc = vorrq_u8(vorrq_u8(vceqq_u8(c, quote), vceqq_u8(c, bslash)), vcltq_u8(c, space));
            // ensure_ascii: bytes >= 0x7f also need escaping (`ff` masks this off otherwise).
            let ha = vbicq_u8(vcgeq_u8(a, del), ff);
            let hc = vbicq_u8(vcgeq_u8(c, del), ff);
            if vmaxvq_u8(vorrq_u8(vorrq_u8(ma, mc), vorrq_u8(ha, hc))) != 0 {
                break;
            }
            i += 32;
        }
        // Exact position within the next 16-byte blocks.
        while i + 16 <= b.len() {
            let a = vld1q_u8(b.as_ptr().add(i));
            let m = vorrq_u8(
                vorrq_u8(vorrq_u8(vceqq_u8(a, quote), vceqq_u8(a, bslash)), vcltq_u8(a, space)),
                vbicq_u8(vcgeq_u8(a, del), ff),
            );
            let nib = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m))));
            if nib != 0 {
                return i + (nib.trailing_zeros() / 4) as usize;
            }
            i += 16;
        }
    }
    i
}

#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn skip_clean_simd(b: &[u8], mut i: usize, ascii: bool) -> usize {
    while i + 8 <= b.len() {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if word_needs_escape(w, ascii) {
            break;
        }
        i += 8;
    }
    i
}

/// Append `s` as a JSON string literal, escaped exactly like Python's
/// `json.dumps` (`ensure_ascii` selects `\uXXXX` for non-ASCII).
pub fn write_json_str(out: &mut String, s: &str, ensure_ascii: bool) {
    out.reserve(s.len() + 2);
    out.push('"');
    let b = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    loop {
        // Fast path: skip clean bytes, 32/16 at a time with NEON, then 8 (SWAR).
        if b.len() - i >= 16 {
            i = skip_clean_simd(b, i, ensure_ascii);
        }
        while i + 8 <= b.len() {
            let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
            if word_needs_escape(w, ensure_ascii) {
                break;
            }
            i += 8;
        }
        if i >= b.len() {
            break;
        }
        let c = b[i];
        // Two-byte escapes are emitted as fixed two-byte pairs (no memcpy call).
        let esc: [u8; 2] = match c {
            b'"' => *b"\\\"",
            b'\\' => *b"\\\\",
            b'\n' => *b"\\n",
            b'\r' => *b"\\r",
            b'\t' => *b"\\t",
            0x08 => *b"\\b",
            0x0c => *b"\\f",
            0x00..=0x1f => [0, 0],
            0x7f..=0xff if ensure_ascii => [0, 0],
            _ => {
                i += 1;
                continue;
            }
        };
        out.push_str(&s[start..i]);
        if esc[0] != 0 {
            // SAFETY: ASCII bytes keep `out` valid UTF-8.
            unsafe { out.as_mut_vec().extend_from_slice(&esc) };
            i += 1;
        } else if c < 0x80 {
            push_u_escape(out, c as u32);
            i += 1;
        } else {
            let ch = s[i..].chars().next().unwrap();
            let cp = ch as u32;
            if cp > 0xffff {
                let v = cp - 0x10000;
                push_u_escape(out, 0xd800 | (v >> 10));
                push_u_escape(out, 0xdc00 | (v & 0x3ff));
            } else {
                push_u_escape(out, cp);
            }
            i += ch.len_utf8();
        }
        start = i;
    }
    if start < b.len() {
        out.push_str(&s[start..]);
    }
    out.push('"');
}

#[inline]
fn push_u_escape(out: &mut String, cp: u32) {
    let buf = [
        b'\\',
        b'u',
        HEX[(cp >> 12 & 0xf) as usize],
        HEX[(cp >> 8 & 0xf) as usize],
        HEX[(cp >> 4 & 0xf) as usize],
        HEX[(cp & 0xf) as usize],
    ];
    // SAFETY-free: all bytes are ASCII.
    out.push_str(std::str::from_utf8(&buf).unwrap());
}

/// Append Python's `repr(f)` as `json.dumps` emits it (`NaN`, `Infinity`,
/// `-Infinity` for non-finite values).
pub fn write_py_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("NaN");
        return;
    }
    if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    if f.is_sign_negative() {
        out.push('-');
    }
    // Rust's `{:e}` is the shortest round-trip representation, like Python's repr.
    let sci = format!("{:e}", f.abs());
    let (mant, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let mut digits = [0u8; 32];
    let mut n = 0;
    for &d in mant.as_bytes() {
        if d != b'.' {
            digits[n] = d;
            n += 1;
        }
    }
    let digits = &digits[..n];
    let decpt = exp + 1;
    let ds = std::str::from_utf8(digits).unwrap();
    if decpt <= -4 || decpt > 16 {
        out.push_str(&ds[..1]);
        if n > 1 {
            out.push('.');
            out.push_str(&ds[1..]);
        }
        let e = decpt - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        let ea = e.unsigned_abs();
        if ea < 10 {
            out.push('0');
        }
        out.push_str(itoa_u64(ea as u64).as_str());
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        out.push_str(ds);
    } else if decpt as usize >= n {
        out.push_str(ds);
        for _ in 0..(decpt as usize - n) {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&ds[..decpt as usize]);
        out.push('.');
        out.push_str(&ds[decpt as usize..]);
    }
}

/// Python `repr(f)` as a `String`.
pub fn py_float_repr(f: f64) -> String {
    let mut s = String::new();
    write_py_float(&mut s, f);
    s
}

pub(crate) struct Itoa {
    buf: [u8; 20],
    start: usize,
}

impl Itoa {
    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[self.start..]).unwrap()
    }
}

pub(crate) fn itoa_u64(mut v: u64) -> Itoa {
    let mut buf = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    Itoa { buf, start: i }
}

pub(crate) fn write_i64(out: &mut String, v: i64) {
    if v < 0 {
        out.push('-');
    }
    out.push_str(itoa_u64(v.unsigned_abs()).as_str());
}

pub(crate) fn write_u64(out: &mut String, v: u64) {
    out.push_str(itoa_u64(v).as_str());
}
