use std::future::Future;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use smol::Timer;

/// Run `fut` until `deadline`; `None` on timeout. Cancel-safe only if `fut` is.
pub(crate) async fn with_deadline<T>(fut: impl Future<Output = T>, deadline: Instant) -> Option<T> {
    smol::future::or(async { Some(fut.await) }, async {
        Timer::at(deadline).await;
        None
    })
    .await
}

/// A random UUIDv4 string (msg ids, session ids).
pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 32 random bytes, hex encoded: a fresh HMAC key per kernel.
pub(crate) fn new_key() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut out = String::with_capacity(64);
    for byte in a.as_bytes().iter().chain(b.as_bytes()) {
        push_hex(&mut out, *byte);
    }
    out
}

pub(crate) fn push_hex(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0xF) as usize] as char);
}

pub(crate) fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    hex.as_chunks::<2>()
        .0
        .iter()
        .map(|[hi, lo]| Some(nibble(*hi)? << 4 | nibble(*lo)?))
        .collect()
}

/// Current UTC time as ISO 8601 with microseconds, e.g. `2026-10-16T09:41:00.123456Z`.
pub(crate) fn now_iso8601() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_iso8601(now.as_secs() as i64, now.subsec_micros())
}

pub(crate) fn format_iso8601(secs: i64, micros: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-since-epoch to proleptic Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Convert a byte offset in `text` to the Unicode code point offset the
/// Jupyter protocol (>= 5.2) uses for `cursor_pos`.
pub fn byte_to_cursor_pos(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    let mut boundary = byte;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].chars().count()
}

/// Convert a Jupyter code point offset back to a byte offset in `text`.
pub fn cursor_pos_to_byte(text: &str, cursor_pos: usize) -> usize {
    text.char_indices()
        .nth(cursor_pos)
        .map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601() {
        assert_eq!(format_iso8601(0, 0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            format_iso8601(951_782_400, 5),
            "2000-02-29T00:00:00.000005Z"
        );
        assert_eq!(
            format_iso8601(1_791_965_096, 123_456),
            "2026-10-14T08:04:56.123456Z"
        );
    }

    #[test]
    fn hex_roundtrip() {
        let mut s = String::new();
        for b in [0u8, 1, 0xab, 0xff] {
            push_hex(&mut s, b);
        }
        assert_eq!(s, "0001abff");
        assert_eq!(decode_hex(s.as_bytes()).unwrap(), vec![0, 1, 0xab, 0xff]);
        assert!(decode_hex(b"abc").is_none());
        assert!(decode_hex(b"zz").is_none());
        assert_eq!(new_key().len(), 64);
    }

    #[test]
    fn cursor_conversions() {
        let s = "é = 1; import o";
        assert_eq!(byte_to_cursor_pos(s, s.len()), s.chars().count());
        assert_eq!(byte_to_cursor_pos(s, 1), 0); // inside 'é'
        assert_eq!(cursor_pos_to_byte(s, 1), 2);
        assert_eq!(cursor_pos_to_byte(s, 1000), s.len());
    }
}
